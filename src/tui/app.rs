//! Application state and event dispatch for the TUI.

use std::sync::Arc;

use futures::StreamExt;
use tokio::sync::mpsc::UnboundedSender;

use strands::Agent;

use super::event::Event;
use crate::commands::{
    self, CommandContext, CommandKind, CommandRegistry, CommandResult,
    DispatchResult, PlanModeAction, SuggestionItem,
};
use crate::mcp::McpSession;

// ---------------------------------------------------------------------------
// State types
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub enum AgentStatus {
    Idle,
    Streaming,
    Error(#[allow(dead_code)] String),
}

#[derive(Clone, Debug)]
pub enum McpStatus {
    /// No MCP config found or not yet started.
    None,
    /// Background loading in progress.
    Loading,
    /// Some servers failed — show warning briefly then disappear.
    Warning { failed: usize, expire_tick: usize },
}

#[derive(Clone, Debug)]
pub enum ContentBlock {
    Text(String),
    ToolCall {
        name: String,
        summary: String,
        status: ToolCallStatus,
        group_key: Option<&'static str>,
    },
}

fn tool_group_key(name: &str) -> Option<&'static str> {
    match name {
        "Read" | "Glob" | "Grep" | "WebFetch" | "WebSearch" => Some("search"),
        "Write" | "Edit" | "NotebookEdit" => Some("write"),
        _ => None,
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ToolCallStatus {
    Running,
    Success,
    Error,
}

#[derive(Clone, Debug)]
pub enum Role {
    User,
    Assistant,
}

#[derive(Clone, Debug)]
pub struct ChatMessage {
    pub role: Role,
    pub blocks: Vec<ContentBlock>,
}

impl ChatMessage {
    pub fn user(text: String) -> Self {
        Self {
            role: Role::User,
            blocks: vec![ContentBlock::Text(text)],
        }
    }

    pub fn assistant_empty() -> Self {
        Self {
            role: Role::Assistant,
            blocks: Vec::new(),
        }
    }

    pub fn append_text(&mut self, delta: &str) {
        if let Some(ContentBlock::Text(ref mut s)) = self.blocks.last_mut() {
            s.push_str(delta);
        } else {
            self.blocks.push(ContentBlock::Text(delta.to_string()));
        }
    }

    pub fn add_tool_call(&mut self, name: String, summary: String, status: ToolCallStatus) {
        let group_key = tool_group_key(&name);
        self.blocks.push(ContentBlock::ToolCall {
            name,
            summary,
            status,
            group_key,
        });
    }

    pub fn set_last_tool_status(&mut self, new_status: ToolCallStatus) {
        for block in self.blocks.iter_mut().rev() {
            if let ContentBlock::ToolCall { status, .. } = block {
                *status = new_status;
                return;
            }
        }
    }

    /// Convert an SDK Message into a display ChatMessage.
    /// Used to rebuild the display list when resuming a session.
    pub fn from_sdk_message(msg: &strands::types::content::Message) -> Self {
        let role = match msg.role {
            strands::types::content::Role::User => Role::User,
            _ => Role::Assistant,
        };
        let mut blocks = Vec::new();
        for block in &msg.content {
            match block {
                strands::types::content::ContentBlock::Text { text, .. } => {
                    blocks.push(ContentBlock::Text(text.clone()));
                }
                strands::types::content::ContentBlock::ToolUse { name, input, .. } => {
                    let summary = crate::repl::tool_call_summary(name, input);
                    blocks.push(ContentBlock::ToolCall {
                        name: name.clone(),
                        summary,
                        status: ToolCallStatus::Success,
                        group_key: tool_group_key(name),
                    });
                }
                strands::types::content::ContentBlock::ToolResult { content, is_error, .. } => {
                    // Show tool results as text (truncated)
                    let text = content.iter().filter_map(|c| {
                        c.text.as_deref()
                    }).collect::<Vec<_>>().join("");
                    if !text.is_empty() {
                        let truncated = if text.len() > 200 {
                            format!("{}…", &text[..199])
                        } else {
                            text
                        };
                        let status = if *is_error { "error" } else { "success" };
                        blocks.push(ContentBlock::Text(format!("[{}: {}]", status, truncated)));
                    }
                }
                _ => {} // Skip Image, Document, etc.
            }
        }
        Self { role, blocks }
    }

    /// Extract all text content from this message's blocks.
    pub fn text_content(&self) -> String {
        self.blocks
            .iter()
            .filter_map(|b| {
                if let ContentBlock::Text(t) = b {
                    Some(t.as_str())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("")
    }
}

/// Text selection state for mouse-based copy.
#[derive(Clone, Debug, Default)]
pub struct Selection {
    /// Whether a selection is in progress (mouse button held).
    pub active: bool,
    /// Anchor point (where mouse was pressed): (row, col) in screen coords.
    pub anchor: (u16, u16),
    /// Current end point (follows mouse drag): (row, col) in screen coords.
    pub end: (u16, u16),
    /// The area of the messages widget (set each render).
    pub messages_area: ratatui::layout::Rect,
    /// Rendered text lines from the last render (for text extraction).
    pub rendered_lines: Vec<String>,
    /// The y-scroll offset used during the last render.
    pub rendered_y_scroll: u16,
}

impl Selection {
    /// Returns (start, end) normalized so start <= end in reading order.
    pub fn ordered(&self) -> ((u16, u16), (u16, u16)) {
        if self.anchor.0 < self.end.0
            || (self.anchor.0 == self.end.0 && self.anchor.1 <= self.end.1)
        {
            (self.anchor, self.end)
        } else {
            (self.end, self.anchor)
        }
    }

    /// Extract the selected text from rendered_lines.
    /// Screen columns map to character (not byte) positions since
    /// ratatui renders one cell per char (wide chars take 2 cells).
    pub fn selected_text(&self) -> String {
        if !self.active && self.anchor == self.end {
            return String::new();
        }
        let ((sr, sc), (er, ec)) = self.ordered();
        let area = self.messages_area;

        // Convert screen coords to line indices relative to rendered content
        let start_line = (sr.saturating_sub(area.y) + self.rendered_y_scroll) as usize;
        let end_line = (er.saturating_sub(area.y) + self.rendered_y_scroll) as usize;

        let mut result = String::new();
        for (i, line_idx) in (start_line..=end_line).enumerate() {
            if line_idx >= self.rendered_lines.len() {
                break;
            }
            let line = &self.rendered_lines[line_idx];
            let char_count = line.chars().count();
            let col_start = if i == 0 {
                sc.saturating_sub(area.x) as usize
            } else {
                0
            };
            let col_end = if line_idx == end_line {
                (ec.saturating_sub(area.x) as usize + 1).min(char_count)
            } else {
                char_count
            };

            if col_start < char_count {
                let end = col_end.min(char_count);
                let substr: String = line.chars().skip(col_start).take(end - col_start).collect();
                result.push_str(&substr);
            }
            if line_idx < end_line {
                result.push('\n');
            }
        }
        result.trim_end().to_string()
    }
}

/// Vim input mode — mirrors Claude Code's vim mode support.
#[derive(Clone, Debug, PartialEq)]
pub enum VimMode {
    Off,
    Normal,
    Insert,
}

/// A pending permission request for a tool call.
#[derive(Clone, Debug)]
pub struct PermissionRequest {
    pub tool_name: String,
    pub tool_input_summary: String,
    /// true = allow, false = deny, None = pending
    pub decision: Option<bool>,
}

/// Per-message render cache entry. Stores pre-rendered lines for a single message.
/// Valid as long as the fingerprint (block_count, text_len, width) matches and
/// the message has no running tool calls (which have animated spinners).
#[derive(Clone)]
pub struct MessageCacheEntry {
    pub lines: Vec<ratatui::text::Line<'static>>,
    pub block_count: usize,
    pub text_len: usize,
    pub width: u16,
    /// Cached visual line count from Paragraph word-wrapping at `wrap_width`.
    pub wrapped_line_count: u16,
    /// The width used to compute `wrapped_line_count`.
    pub wrap_width: u16,
}

impl MessageCacheEntry {
    pub fn new(lines: Vec<ratatui::text::Line<'static>>, msg: &ChatMessage, width: u16) -> Self {
        Self {
            lines,
            block_count: msg.blocks.len(),
            text_len: msg_text_len(msg),
            width,
            wrapped_line_count: 0,
            wrap_width: 0,
        }
    }

    pub fn is_valid(&self, msg: &ChatMessage, width: u16) -> bool {
        self.width == width
            && self.block_count == msg.blocks.len()
            && self.text_len == msg_text_len(msg)
            && !msg_has_running_tool(msg)
    }
}

fn msg_text_len(msg: &ChatMessage) -> usize {
    msg.blocks.iter().map(|b| match b {
        ContentBlock::Text(t) => t.len(),
        _ => 0,
    }).sum()
}

fn msg_has_running_tool(msg: &ChatMessage) -> bool {
    msg.blocks.iter().any(|b| matches!(b, ContentBlock::ToolCall { status: ToolCallStatus::Running, .. }))
}

/// Incremental markdown cache for the streaming message's last text block.
/// Tracks a "stable prefix boundary" — only the unstable suffix after the
/// last complete markdown block is re-parsed each frame. Mirrors Claude Code's
/// `StreamingMarkdown` component with `stablePrefixRef`.
#[derive(Clone)]
pub struct StreamingMdCache {
    /// Byte offset in the text where the stable prefix ends.
    pub boundary: usize,
    /// Rendered lines for text[..boundary].
    pub prefix_lines: Vec<ratatui::text::Line<'static>>,
}

pub struct AppState {
    pub messages: Vec<ChatMessage>,
    pub scroll_offset: u16,
    pub auto_scroll: bool,
    pub input: tui_textarea::TextArea<'static>,
    pub agent_status: AgentStatus,
    pub tick_count: usize,
    pub model_name: String,
    pub should_quit: bool,
    pub total_lines: u16,
    pub terminal_width: u16,
    pub turn_count: usize,
    pub input_history: Vec<String>,
    pub history_index: Option<usize>,
    pub history_stash: String,
    /// Clone of the agent used to cancel a streaming task.
    pub cancel_agent: Option<Agent>,
    /// Slash command registry.
    pub command_registry: CommandRegistry,
    /// Autocomplete suggestions for the current input.
    pub suggestions: Vec<SuggestionItem>,
    /// Index of the selected suggestion (-1 = none). Mirrors Claude Code's `selectedSuggestion`.
    pub selected_suggestion: i32,
    /// Connected MCP server info (for /mcp command).
    pub mcp_servers: Vec<crate::mcp::McpServerInfo>,
    /// MCP loading status for the status bar.
    pub mcp_status: McpStatus,
    /// Text selection state for mouse copy.
    pub selection: Selection,

    // --- New features (closing gaps with Claude Code) ---

    /// Vim mode state (Off/Normal/Insert).
    pub vim_mode: VimMode,
    /// Unseen messages: line index where the divider should render.
    /// Set when user scrolls away from bottom during streaming.
    pub unseen_from_line: Option<usize>,
    /// Count of unseen messages since user scrolled away.
    pub unseen_message_count: usize,
    /// Pending permission request overlay.
    pub permission_request: Option<PermissionRequest>,
    /// Per-message render cache. Parallel to `messages` — each entry caches the
    /// rendered lines for the corresponding message, validated by fingerprint.
    pub message_cache: Vec<Option<MessageCacheEntry>>,
    /// Incremental markdown cache for the streaming message's last text block.
    pub streaming_md_cache: Option<StreamingMdCache>,
    /// Whether running in fullscreen (alt-screen) mode.
    #[allow(dead_code)]
    pub fullscreen: bool,
    /// Typeahead prediction text shown dimmed in input bar.
    pub typeahead: Option<String>,
    /// Configurable keybindings (action → key chord).
    #[allow(dead_code)]
    pub keybindings: super::keybindings::KeybindingMap,
    /// Tick count when Ctrl+C was last pressed (for double-tap quit).
    pub last_ctrl_c_tick: Option<usize>,
    /// Current permission mode (cycles via Shift+Tab).
    pub permission_mode: PermissionMode,

    // --- Context window tracking (matching Claude Code) ---

    /// Token usage percentage (0.0–100.0). None if not tracking.
    pub context_percent_used: Option<f64>,
    /// True if above warning threshold (~90% context used).
    pub context_warning: bool,
    /// True if at hard blocking limit (~98% context used).
    pub context_critical: bool,
    /// Raw token counts: (used, limit).
    pub token_counts: Option<(u64, u64)>,
    /// Whether the current stream is a /compact — on completion, replace history.
    pub pending_compact: bool,
    /// Pending system-reminder to inject into the next user prompt.
    /// Set when entering plan mode; consumed (taken) on the next agent call.
    pub pending_system_reminder: Option<String>,

    // --- Context analysis data (set once at startup for /context command) ---

    /// System prompt text (for token estimation in /context).
    pub system_prompt_text: String,
    /// Tool spec summaries (for token estimation in /context).
    pub tool_spec_summaries: Vec<crate::context::ToolSpecSummary>,
    /// Memory files: (path, source_type, content) for /context.
    pub memory_files: Vec<(String, String, String)>,
    /// Loaded skills for /context.
    pub skill_summaries: Vec<crate::context::SkillSummary>,

    // --- Session persistence ---

    // --- Rewind ---

    /// Tick count when Esc was last pressed on empty input (for double-tap rewind).
    pub last_esc_tick: Option<usize>,

    /// Current session ID (displayed in status bar, used for /session commands).
    pub session_id: Option<String>,
    /// Session title (custom or AI-generated). Displayed in status bar.
    /// Priority: custom_title > ai_title > None.
    pub session_title: Option<String>,

    // --- Plan mode decision UI ---

    /// When set, the suggestion dropdown shows plan mode choices and Esc re-injects them.
    pub awaiting_plan_decision: Option<PlanDecisionKind>,
    /// Stored plan content when awaiting exit decision.
    pub pending_plan_content: Option<String>,
    /// Stored plan file path when awaiting exit decision.
    pub pending_plan_file: Option<String>,
    /// When true, suppress future EnterPlanModeRequested popups (user already rejected).
    pub plan_mode_enter_rejected: bool,
    /// When true, suppress future PlanModeExitRequested popups (user already approved/handled).
    pub plan_mode_exit_handled: bool,
}

/// Whether we're awaiting a plan mode decision from the user.
#[derive(Clone, Debug)]
pub enum PlanDecisionKind {
    /// Waiting for user to confirm/reject entering plan mode.
    Enter,
    /// Waiting for user to approve/reject the plan.
    Exit,
}

/// Permission modes matching Claude Code's Shift+Tab cycle.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[allow(dead_code)]
pub enum PermissionMode {
    /// Default — tools require per-use permission.
    #[default]
    Default,
    /// Plan — read-only exploration, no file writes.
    Plan,
    /// Accept edits — auto-approve file edits.
    AcceptEdits,
    /// Bypass — auto-approve everything.
    BypassPermissions,
}

#[allow(dead_code)]
impl PermissionMode {
    /// Cycle to the next mode (Shift+Tab order matches Claude Code).
    pub fn next(&self) -> Self {
        match self {
            PermissionMode::Default => PermissionMode::Plan,
            PermissionMode::Plan => PermissionMode::AcceptEdits,
            PermissionMode::AcceptEdits => PermissionMode::BypassPermissions,
            PermissionMode::BypassPermissions => PermissionMode::Default,
        }
    }

    /// Display label for the status bar.
    pub fn label(&self) -> &'static str {
        match self {
            PermissionMode::Default => "Default",
            PermissionMode::Plan => "Plan",
            PermissionMode::AcceptEdits => "Auto-edit",
            PermissionMode::BypassPermissions => "YOLO",
        }
    }

    /// Parse a mode name (as returned by `CommandResult::ModeSwitch`) into a `PermissionMode`.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "default" => Some(PermissionMode::Default),
            "plan" => Some(PermissionMode::Plan),
            "accept-edits" => Some(PermissionMode::AcceptEdits),
            "bypass" => Some(PermissionMode::BypassPermissions),
            _ => None,
        }
    }

    /// Color for the status bar badge.
    pub fn color(&self) -> ratatui::style::Color {
        use ratatui::style::Color;
        match self {
            PermissionMode::Default => Color::DarkGray,
            PermissionMode::Plan => Color::Blue,
            PermissionMode::AcceptEdits => Color::Yellow,
            PermissionMode::BypassPermissions => Color::Red,
        }
    }
}

impl AppState {
    pub fn new(model_name: String, command_registry: CommandRegistry, mcp_servers: Vec<crate::mcp::McpServerInfo>) -> Self {
        let mut input = tui_textarea::TextArea::default();
        input.set_cursor_line_style(ratatui::style::Style::default());
        input.set_placeholder_text(" ");
        let terminal_width = crossterm::terminal::size().map(|(w, _)| w).unwrap_or(80);
        let keybindings = super::keybindings::load_keybindings();
        let fullscreen = std::env::var("STRANDS_NO_FULLSCREEN").map(|v| v != "1").unwrap_or(true);
        Self {
            messages: Vec::new(),
            scroll_offset: 0,
            auto_scroll: true,
            input,
            agent_status: AgentStatus::Idle,
            tick_count: 0,
            model_name,
            should_quit: false,
            total_lines: 0,
            terminal_width,
            turn_count: 0,
            input_history: Vec::new(),
            history_index: None,
            history_stash: String::new(),
            cancel_agent: None,
            command_registry,
            suggestions: Vec::new(),
            selected_suggestion: -1,
            mcp_servers,
            mcp_status: McpStatus::Loading,
            selection: Selection::default(),
            vim_mode: VimMode::Off,
            unseen_from_line: None,
            unseen_message_count: 0,
            permission_request: None,
            message_cache: Vec::new(),
            streaming_md_cache: None,
            fullscreen,
            typeahead: None,
            keybindings,
            last_ctrl_c_tick: None,
            permission_mode: PermissionMode::Default,
            context_percent_used: None,
            context_warning: false,
            context_critical: false,
            token_counts: None,
            pending_compact: false,
            pending_system_reminder: None,
            system_prompt_text: String::new(),
            tool_spec_summaries: Vec::new(),
            memory_files: Vec::new(),
            skill_summaries: Vec::new(),
            last_esc_tick: None,
            session_id: None,
            session_title: None,
            awaiting_plan_decision: None,
            pending_plan_content: None,
            pending_plan_file: None,
            plan_mode_enter_rejected: false,
            plan_mode_exit_handled: false,
        }
    }

    /// Clear all render caches (used on /clear).
    pub fn clear_render_caches(&mut self) {
        self.message_cache.clear();
        self.streaming_md_cache = None;
    }
}

// ---------------------------------------------------------------------------
// TUI App
// ---------------------------------------------------------------------------

pub struct TuiApp {
    pub state: AppState,
    agent: Agent,
    /// Model reference for background tasks (e.g. AI title generation).
    model: Arc<dyn strands::types::models::Model>,
    /// MCP clients kept alive for session duration (Drop kills subprocesses / closes HTTP sessions).
    #[allow(dead_code)]
    _mcp_clients: Option<(Vec<strands::tools::mcp::MCPClient>, Vec<strands::tools::mcp::MCPHttpClient>)>,
}

impl TuiApp {
    pub fn new(agent: Agent, model_name: String, command_registry: CommandRegistry, model: Arc<dyn strands::types::models::Model>) -> Self {
        Self {
            state: AppState::new(model_name, command_registry, Vec::new()),
            agent,
            model,
            _mcp_clients: None,
        }
    }

    /// Get a reference to the underlying agent (for hook registration in tests).
    #[cfg(test)]
    pub fn agent_ref(&self) -> &Agent {
        &self.agent
    }

    /// Absorb a loaded MCP session — registers tools on the agent at runtime
    /// and stores clients to keep subprocesses / HTTP sessions alive.
    pub fn apply_mcp_session(&mut self, session: McpSession) {
        let failed = session.failed_count;
        for tool in &session.tools {
            self.agent.add_tool(tool.clone());
        }
        self.state.mcp_servers = session.servers;
        self._mcp_clients = Some((session.stdio_clients, session.http_clients));
        if failed > 0 {
            // Show warning for ~2 seconds (tick rate = 12 Hz → 24 ticks)
            self.state.mcp_status = McpStatus::Warning {
                failed,
                expire_tick: self.state.tick_count + 24,
            };
        } else {
            self.state.mcp_status = McpStatus::None;
        }
    }

    /// Submit the current input to the agent (spawns a streaming task).
    pub fn submit(&mut self, event_tx: UnboundedSender<Event>) {
        let lines: Vec<String> = self.state.input.lines().iter().map(|s| s.to_string()).collect();
        let prompt = lines.join("\n");
        if prompt.trim().is_empty() {
            return;
        }

        // Handle slash commands via registry dispatch
        let trimmed = prompt.trim();
        if trimmed.starts_with('/') {
            // Build messages JSON from agent for /context analysis
            let messages_json: Vec<serde_json::Value> = self
                .agent
                .get_messages()
                .iter()
                .filter_map(|m| serde_json::to_value(m).ok())
                .collect();

            // Build MCP tool specs from server info
            let mcp_tool_specs: Vec<(String, String, String)> = self
                .state
                .mcp_servers
                .iter()
                .flat_map(|s| {
                    s.tool_names.iter().map(move |t| {
                        (t.clone(), s.name.clone(), format!("{{\"name\":\"{}\"}}", t))
                    })
                })
                .collect();

            let ctx = CommandContext {
                model_name: self.state.model_name.clone(),
                turn_count: self.state.turn_count,
                message_count: self.state.messages.len(),
                all_commands: self.state.command_registry.command_infos(),
                mcp_servers: self.state.mcp_servers.clone(),
                token_counts: self.state.token_counts,
                context_percent_used: self.state.context_percent_used,
                system_prompt: self.state.system_prompt_text.clone(),
                tool_specs: self.state.tool_spec_summaries.clone(),
                mcp_tool_specs,
                memory_files: self.state.memory_files.clone(),
                skills: self.state.skill_summaries.clone(),
                messages_json,
            };
            match commands::dispatch(trimmed, &self.state.command_registry, &ctx) {
                DispatchResult::Local(CommandResult::Quit) => {
                    self.state.should_quit = true;
                    return;
                }
                DispatchResult::Local(CommandResult::Clear) => {
                    self.state.messages.clear();
                    self.state.clear_render_caches();
                    self.agent.clear_history();
                    strands_tools::file::file_history::clear();
                    self.reset_input();
                    return;
                }
                DispatchResult::Local(CommandResult::Text(text)) => {
                    self.state.messages.push(ChatMessage::user(trimmed.to_string()));
                    let mut msg = ChatMessage::assistant_empty();
                    msg.append_text(&text);
                    self.state.messages.push(msg);

                    self.state.auto_scroll = true;
                    self.state.scroll_offset = 0;
                    self.reset_input();
                    return;
                }
                DispatchResult::Local(CommandResult::Skip) => {
                    self.reset_input();
                    return;
                }
                DispatchResult::Local(CommandResult::ModelPicker { .. }) => {
                    // Set input to "/model " and show model suggestions inline
                    self.set_input("/model ");
                    self.update_suggestions();
                    return;
                }
                DispatchResult::Local(CommandResult::SessionPicker) => {
                    self.set_input("/resume ");
                    self.update_suggestions();
                    return;
                }
                DispatchResult::Local(CommandResult::Rewind) => {
                    self.set_input("/rewind ");
                    self.update_suggestions();
                    return;
                }
                DispatchResult::Local(CommandResult::SwitchModel(model_id)) => {
                    self.reset_input();
                    self.switch_model(model_id, event_tx);
                    return;
                }
                DispatchResult::Local(CommandResult::ResumeSession(session_ref)) => {
                    self.reset_input();
                    self.resume_session(session_ref, event_tx);
                    return;
                }
                DispatchResult::Local(CommandResult::ModeSwitch(mode_name)) => {
                    self.apply_mode_switch(&mode_name);
                    self.reset_input();
                    return;
                }
                DispatchResult::Local(CommandResult::SetSessionTitle(title)) => {
                    if let Some(journal) = crate::session::get_journal() {
                        let journal = std::sync::Arc::clone(journal);
                        let t = title.clone();
                        tokio::spawn(async move { let _ = journal.set_custom_title(t).await; });
                    }
                    self.state.session_title = Some(title.clone());
                    self.state.messages.push(ChatMessage::user(trimmed.to_string()));
                    let mut msg = ChatMessage::assistant_empty();
                    msg.append_text(&format!("Session renamed to: {}", title));
                    self.state.messages.push(msg);
                    self.state.auto_scroll = true;
                    self.state.scroll_offset = 0;
                    self.reset_input();
                    return;
                }
                DispatchResult::Local(CommandResult::GenerateSessionTitle) => {
                    self.state.messages.push(ChatMessage::user(trimmed.to_string()));
                    let mut msg = ChatMessage::assistant_empty();
                    msg.append_text("Generating session title...");
                    self.state.messages.push(msg);
                    self.state.auto_scroll = true;
                    self.state.scroll_offset = 0;
                    self.reset_input();
                    self.trigger_ai_title_generation(event_tx);
                    return;
                }
                DispatchResult::Prompt(expanded) => {
                    // Show the original command as user message, send expanded prompt to model
                    self.state.messages.push(ChatMessage::user(trimmed.to_string()));
                    self.state.messages.push(ChatMessage::assistant_empty());
                    self.state.agent_status = AgentStatus::Streaming;
                    self.state.streaming_md_cache = None;
                    self.state.auto_scroll = true;
                    self.state.scroll_offset = 0;
                    self.state.turn_count += 1;
                    self.reset_input();
                    self.agent.reset_cancel();
                    self.state.cancel_agent = Some(self.agent.clone());
                    let agent = self.agent.clone();
                    tokio::spawn(async move {
                        Self::run_agent_stream(agent, &expanded, event_tx).await;
                    });
                    return;
                }
                DispatchResult::CompactPrompt(expanded) => {
                    // Send summary prompt to model; on AgentDone, replace history with response
                    self.state.pending_compact = true;
                    self.state.messages.push(ChatMessage::user(trimmed.to_string()));
                    self.state.messages.push(ChatMessage::assistant_empty());
                    self.state.agent_status = AgentStatus::Streaming;
                    self.state.streaming_md_cache = None;
                    self.state.auto_scroll = true;
                    self.state.scroll_offset = 0;
                    self.state.turn_count += 1;
                    self.reset_input();
                    self.agent.reset_cancel();
                    self.state.cancel_agent = Some(self.agent.clone());
                    let agent = self.agent.clone();
                    tokio::spawn(async move {
                        Self::run_agent_stream(agent, &expanded, event_tx).await;
                    });
                    return;
                }
                DispatchResult::Unknown(name) => {
                    self.state.messages.push(ChatMessage::user(trimmed.to_string()));
                    let mut msg = ChatMessage::assistant_empty();
                    msg.append_text(&format!("Unknown command: /{}. Type /help for available commands.", name));
                    self.state.messages.push(msg);

                    self.state.auto_scroll = true;
                    self.state.scroll_offset = 0;
                    self.reset_input();
                    return;
                }
                DispatchResult::NotACommand => {
                    // Fall through — treat as normal input to the model
                }
            }
        }

        // Save to input history (deduplicated)
        let trimmed = prompt.trim().to_string();
        if !trimmed.starts_with('/') {
            if self.state.input_history.last().map(|s| s.as_str()) != Some(trimmed.as_str()) {
                self.state.input_history.push(trimmed);
            }
        }
        self.state.history_index = None;
        self.state.history_stash.clear();
        self.state.turn_count += 1;

        // Record last prompt and git branch in session journal (fire-and-forget)
        if let Some(journal) = crate::session::get_journal() {
            let journal = std::sync::Arc::clone(journal);
            let prompt_text = prompt.clone();
            tokio::spawn(async move {
                let _ = journal.set_last_prompt(prompt_text).await;
                // Record current git branch (matches TypeScript per-turn stamping)
                if let Some(ctx) = crate::context::get_git_status(
                    &std::env::current_dir().unwrap_or_default(),
                ) {
                    let _ = journal.set_git_branch(ctx.branch).await;
                }
            });
        }

        // Create file history snapshot for rewind support.
        // The snapshot ID = "msg-{index}" matches the convention in open_message_selector.
        let msg_index = self.state.messages.len();
        strands_tools::file::file_history::make_snapshot(&format!("msg-{}", msg_index));

        // Add user message (shown in UI as-is)
        self.state.messages.push(ChatMessage::user(prompt.clone()));
        self.state.messages.push(ChatMessage::assistant_empty());
        self.state.agent_status = AgentStatus::Streaming;
        self.state.streaming_md_cache = None; // reset for new streaming session
        self.state.auto_scroll = true;
        self.state.scroll_offset = 0;

        self.reset_input();

        // Prepend any pending system-reminder (e.g. plan mode instructions) to the
        // agent prompt. The user sees their original message in the UI, but the
        // agent receives the system-reminder context ahead of the user's text.
        let agent_prompt = if let Some(reminder) = self.state.pending_system_reminder.take() {
            format!("{}\n\n{}", reminder, prompt)
        } else {
            prompt
        };

        // Reset cancel signal and store agent clone for cancellation
        self.agent.reset_cancel();
        self.state.cancel_agent = Some(self.agent.clone());

        // Auto-generate session title on first user message (matches Claude Code behavior)
        let first_msg_for_title = if self.state.turn_count == 1 && self.state.session_title.is_none() {
            Some(agent_prompt.chars().take(500).collect::<String>())
        } else {
            None
        };

        // Spawn agent streaming task
        let agent = self.agent.clone();
        let event_tx_for_title = event_tx.clone();
        tokio::spawn(async move {
            Self::run_agent_stream(agent, &agent_prompt, event_tx).await;
        });

        if let Some(first_msg) = first_msg_for_title {
            let model = Arc::clone(&self.model);
            tokio::spawn(async move {
                if let Some(title) = crate::title_generator::generate_session_title(&first_msg, model).await {
                    let _ = event_tx_for_title.send(Event::AiTitleGenerated(title));
                }
            });
        }
    }

    /// Try to execute an immediate slash command while the agent is streaming.
    /// Mirrors Claude Code's `handlePromptSubmit` fast-path for `immediate: true` commands.
    /// Only local commands with `immediate: true` are allowed; everything else is ignored.
    pub fn try_immediate_command(&mut self) {
        let lines: Vec<String> = self.state.input.lines().iter().map(|s| s.to_string()).collect();
        let prompt = lines.join("\n");
        let trimmed = prompt.trim();

        if !trimmed.starts_with('/') {
            return;
        }

        let parsed = match commands::parse_slash_command(trimmed) {
            Some(p) => p,
            None => return,
        };

        let cmd = match self.state.command_registry.find(&parsed.command_name) {
            Some(c) => c,
            None => return,
        };

        // Only immediate local commands bypass the streaming guard
        if !cmd.immediate {
            return;
        }
        if !matches!(cmd.kind, CommandKind::Local { .. }) {
            return;
        }

        let messages_json: Vec<serde_json::Value> = self
            .agent
            .get_messages()
            .iter()
            .filter_map(|m| serde_json::to_value(m).ok())
            .collect();
        let mcp_tool_specs: Vec<(String, String, String)> = self
            .state
            .mcp_servers
            .iter()
            .flat_map(|s| {
                s.tool_names.iter().map(move |t| {
                    (t.clone(), s.name.clone(), format!("{{\"name\":\"{}\"}}", t))
                })
            })
            .collect();
        let ctx = CommandContext {
            model_name: self.state.model_name.clone(),
            turn_count: self.state.turn_count,
            message_count: self.state.messages.len(),
            all_commands: self.state.command_registry.command_infos(),
            mcp_servers: self.state.mcp_servers.clone(),
            token_counts: self.state.token_counts,
            context_percent_used: self.state.context_percent_used,
            system_prompt: self.state.system_prompt_text.clone(),
            tool_specs: self.state.tool_spec_summaries.clone(),
            mcp_tool_specs,
            memory_files: self.state.memory_files.clone(),
            skills: self.state.skill_summaries.clone(),
            messages_json,
        };

        match commands::dispatch(trimmed, &self.state.command_registry, &ctx) {
            DispatchResult::Local(CommandResult::Quit) => {
                // Cancel the streaming agent first
                if let Some(ref a) = self.state.cancel_agent {
                    a.cancel();
                }
                self.state.should_quit = true;
            }
            DispatchResult::Local(CommandResult::Clear) => {
                if let Some(ref a) = self.state.cancel_agent {
                    a.cancel();
                }
                self.state.messages.clear();
                self.state.clear_render_caches();
                self.agent.clear_history();
                strands_tools::file::file_history::clear();
                self.state.agent_status = AgentStatus::Idle;
                self.state.cancel_agent = None;
                self.reset_input();
            }
            DispatchResult::Local(CommandResult::Text(text)) => {
                self.state.messages.push(ChatMessage::user(trimmed.to_string()));
                let mut msg = ChatMessage::assistant_empty();
                msg.append_text(&text);
                self.state.messages.push(msg);
                self.state.auto_scroll = true;
                self.state.scroll_offset = 0;
                self.reset_input();
            }
            DispatchResult::Local(CommandResult::Skip) => {
                self.reset_input();
            }
            DispatchResult::Local(CommandResult::ModeSwitch(mode_name)) => {
                self.apply_mode_switch(&mode_name);
                self.reset_input();
            }
            DispatchResult::Local(CommandResult::SetSessionTitle(title)) => {
                if let Some(journal) = crate::session::get_journal() {
                    let journal = std::sync::Arc::clone(journal);
                    let t = title.clone();
                    tokio::spawn(async move { let _ = journal.set_custom_title(t).await; });
                }
                self.state.session_title = Some(title.clone());
                self.state.messages.push(ChatMessage::user(trimmed.to_string()));
                let mut msg = ChatMessage::assistant_empty();
                msg.append_text(&format!("Session renamed to: {}", title));
                self.state.messages.push(msg);
                self.state.auto_scroll = true;
                self.state.scroll_offset = 0;
                self.reset_input();
            }
            _ => {} // Non-local or prompt commands are not allowed during streaming
        }
    }

    /// Reset the input textarea to its default state.
    /// Switch the model — shows a message and spawns async build+swap.
    /// Used by both `/model <alias>` dispatch and the interactive picker.
    /// Apply a mode switch from /plan, /default, /accept-edits, /bypass commands
    /// or from Shift+Tab cycling.
    pub fn apply_mode_switch(&mut self, mode_name: &str) {
        // Clear any pending plan mode decision state — Shift+Tab overrides the popup.
        if self.state.awaiting_plan_decision.is_some() {
            self.state.awaiting_plan_decision = None;
            self.state.suggestions.clear();
            self.state.selected_suggestion = -1;
            self.state.pending_plan_content = None;
            self.state.pending_plan_file = None;
        }

        let (new_mode, tools_mode) = match mode_name {
            "plan" => {
                if let Err(e) = strands_tools::utility::plan_state::enter_plan_mode(None) {
                    let mut msg = ChatMessage::assistant_empty();
                    msg.append_text(&format!("Cannot enter plan mode: {}", e));
                    self.state.messages.push(msg);
                    return;
                }
                // Reset suppression flags for new plan session
                self.state.plan_mode_enter_rejected = false;
                self.state.plan_mode_exit_handled = false;
                // Enable deferred mode — tools return success without mutating state,
                // TUI handles actual transitions after user approval.
                strands_tools::utility::plan_state::set_deferred(true);
                // Store system-reminder for injection into next agent call
                let plan_file = strands_tools::utility::plan_state::get_plan_file_path(None);
                let reminder = strands_tools::utility::plan_state::build_plan_mode_system_reminder(&plan_file);
                self.state.pending_system_reminder = Some(reminder);
                (PermissionMode::Plan, None)
            }
            "default" => (PermissionMode::Default, Some(strands_tools::utility::plan_state::PermissionMode::Default)),
            "accept-edits" => (PermissionMode::AcceptEdits, Some(strands_tools::utility::plan_state::PermissionMode::AcceptEdits)),
            "bypass" => (PermissionMode::BypassPermissions, Some(strands_tools::utility::plan_state::PermissionMode::BypassPermissions)),
            _ => return,
        };

        let old_label = self.state.permission_mode.label();

        // If leaving plan mode, exit cleanly
        if self.state.permission_mode == PermissionMode::Plan && new_mode != PermissionMode::Plan {
            strands_tools::utility::plan_state::set_deferred(false);
            let _ = strands_tools::utility::plan_state::exit_plan_mode(None);
        }

        // Set the tools-layer mode for non-plan modes
        if let Some(tm) = tools_mode {
            strands_tools::utility::plan_state::set_permission_mode(tm);
        }

        self.state.permission_mode = new_mode.clone();

        let mut msg = ChatMessage::assistant_empty();
        msg.append_text(&format!("Switched to {} mode (was: {})", new_mode.label(), old_label));
        self.state.messages.push(msg);
        self.state.auto_scroll = true;
        self.state.scroll_offset = 0;
    }

    pub fn switch_model(
        &mut self,
        model_id: String,
        event_tx: tokio::sync::mpsc::UnboundedSender<Event>,
    ) {
        self.state.messages.push(ChatMessage::user(format!("/model {}", model_id)));
        let mut msg = ChatMessage::assistant_empty();
        msg.append_text(&format!("Switching model to {}...", model_id));
        self.state.messages.push(msg);
        self.state.auto_scroll = true;
        self.state.scroll_offset = 0;

        let agent = self.agent.clone();
        let event_tx_clone = event_tx.clone();
        let model_id_clone = model_id.clone();
        tokio::spawn(async move {
            match crate::build_model_by_id(&model_id_clone).await {
                Ok(new_model) => {
                    agent.swap_model(new_model);
                    let _ = event_tx_clone.send(Event::AgentTextDelta(
                        format!("\nModel switched to {}", model_id_clone),
                    ));
                    let _ = event_tx_clone.send(Event::AgentDone);
                }
                Err(e) => {
                    let _ = event_tx_clone.send(Event::AgentError(
                        format!("Failed to switch model: {}", e),
                    ));
                    let _ = event_tx_clone.send(Event::AgentDone);
                }
            }
        });
        self.state.model_name = model_id;
        self.state.agent_status = AgentStatus::Streaming;
    }

    pub fn reset_input(&mut self) {
        self.state.input = tui_textarea::TextArea::default();
        self.state.input.set_cursor_line_style(ratatui::style::Style::default());
        self.state.input.set_placeholder_text("/help");
        self.state.suggestions.clear();
        self.state.selected_suggestion = -1;
    }

    /// Set the input textarea to a specific string.
    pub fn set_input(&mut self, text: &str) {
        self.state.input = tui_textarea::TextArea::default();
        self.state.input.set_cursor_line_style(ratatui::style::Style::default());
        self.state.input.set_placeholder_text("/help");
        for ch in text.chars() {
            self.state.input.insert_char(ch);
        }
    }

    /// Update autocomplete suggestions based on current input.
    /// Called after every keystroke. Mirrors Claude Code's `updateSuggestions`.
    pub fn update_suggestions(&mut self) {
        let text = self.state.input.lines().join("\n");
        let trimmed = text.trim();

        // `/rewind` — generate rewind suggestions from message history (like /resume)
        let new_suggestions = if trimmed == "/rewind" || trimmed == "/checkpoint"
            || trimmed.starts_with("/rewind ") || trimmed.starts_with("/checkpoint ")
        {
            let query = trimmed
                .strip_prefix("/rewind")
                .or_else(|| trimmed.strip_prefix("/checkpoint"))
                .unwrap_or("")
                .trim()
                .to_lowercase();
            self.generate_rewind_suggestions(&query)
        } else {
            commands::generate_suggestions(&text, &self.state.command_registry, &self.state.model_name)
        };

        if new_suggestions.is_empty() {
            self.state.suggestions.clear();
            self.state.selected_suggestion = -1;
        } else {
            // Preserve selection if the same item is still in the list
            // (mirrors Claude Code's getPreservedSelection)
            let prev_name = if self.state.selected_suggestion >= 0
                && (self.state.selected_suggestion as usize) < self.state.suggestions.len()
            {
                Some(
                    self.state.suggestions[self.state.selected_suggestion as usize]
                        .name
                        .clone(),
                )
            } else {
                None
            };

            self.state.suggestions = new_suggestions;

            if let Some(ref prev) = prev_name {
                if let Some(idx) = self.state.suggestions.iter().position(|s| s.name == *prev) {
                    self.state.selected_suggestion = idx as i32;
                } else {
                    self.state.selected_suggestion = 0;
                }
            } else {
                self.state.selected_suggestion = 0;
            }
        }
    }

    /// Accept the currently selected suggestion — replace input with `/command `.
    /// For model suggestions, fills `/model <alias> ` instead.
    /// Mirrors Claude Code's `applyCommandSuggestion`.
    pub fn accept_suggestion(&mut self) {
        if self.state.selected_suggestion < 0
            || self.state.selected_suggestion as usize >= self.state.suggestions.len()
        {
            return;
        }
        let suggestion = self.state.suggestions[self.state.selected_suggestion as usize].clone();

        let new_input = if suggestion.model_id.is_some() {
            format!("/model {} ", suggestion.name)
        } else if let Some(ref sid) = suggestion.session_id {
            format!("/resume {} ", sid)
        } else {
            format!("/{} ", suggestion.name)
        };

        self.set_input(&new_input);

        // Clear suggestions after accepting
        self.state.suggestions.clear();
        self.state.selected_suggestion = -1;
    }

    /// If the selected suggestion is a model, return its model_id for direct switching.
    pub fn selected_model_id(&self) -> Option<String> {
        if self.state.selected_suggestion < 0
            || self.state.selected_suggestion as usize >= self.state.suggestions.len()
        {
            return None;
        }
        self.state.suggestions[self.state.selected_suggestion as usize]
            .model_id
            .clone()
    }

    /// If the selected suggestion is a session, return its session_id for direct resume.
    pub fn selected_session_id(&self) -> Option<String> {
        if self.state.selected_suggestion < 0
            || self.state.selected_suggestion as usize >= self.state.suggestions.len()
        {
            return None;
        }
        self.state.suggestions[self.state.selected_suggestion as usize]
            .session_id
            .clone()
    }

    /// If the selected suggestion is a rewind target, return (message_index, message_id).
    pub fn selected_rewind_info(&self) -> Option<(usize, String)> {
        if self.state.selected_suggestion < 0
            || self.state.selected_suggestion as usize >= self.state.suggestions.len()
        {
            return None;
        }
        self.state.suggestions[self.state.selected_suggestion as usize]
            .rewind_info
            .clone()
    }

    /// If the selected suggestion is a plan mode action, return it.
    pub fn selected_plan_mode_action(&self) -> Option<PlanModeAction> {
        if self.state.selected_suggestion < 0
            || self.state.selected_suggestion as usize >= self.state.suggestions.len()
        {
            return None;
        }
        self.state.suggestions[self.state.selected_suggestion as usize]
            .plan_mode_action
            .clone()
    }

    /// Populate suggestions with plan mode enter options.
    pub fn show_enter_plan_suggestions(&mut self) {
        self.state.suggestions = vec![
            SuggestionItem {
                name: "Yes, enter plan mode".to_string(),
                description: "Explore codebase and design approach before coding".to_string(),
                model_id: None,
                session_id: None,
                rewind_info: None,
                plan_mode_action: Some(PlanModeAction::ConfirmEnter),
                no_slash_prefix: true,
            },
            SuggestionItem {
                name: "No, start implementing now".to_string(),
                description: "Skip planning, start coding directly".to_string(),
                model_id: None,
                session_id: None,
                rewind_info: None,
                plan_mode_action: Some(PlanModeAction::RejectEnter),
                no_slash_prefix: true,
            },
        ];
        self.state.selected_suggestion = 0;
        self.state.awaiting_plan_decision = Some(PlanDecisionKind::Enter);
    }

    /// Populate suggestions with plan mode exit/approval options.
    pub fn show_exit_plan_suggestions(&mut self) {
        self.state.suggestions = vec![
            SuggestionItem {
                name: "Yes, auto-accept edits".to_string(),
                description: "Approve plan and auto-accept file edits".to_string(),
                model_id: None,
                session_id: None,
                rewind_info: None,
                plan_mode_action: Some(PlanModeAction::ApproveAcceptEdits),
                no_slash_prefix: true,
            },
            SuggestionItem {
                name: "Yes, manually approve edits".to_string(),
                description: "Approve plan and confirm each edit".to_string(),
                model_id: None,
                session_id: None,
                rewind_info: None,
                plan_mode_action: Some(PlanModeAction::ApproveManual),
                no_slash_prefix: true,
            },
            SuggestionItem {
                name: "No, keep planning".to_string(),
                description: "Stay in plan mode — type feedback below".to_string(),
                model_id: None,
                session_id: None,
                rewind_info: None,
                plan_mode_action: Some(PlanModeAction::KeepPlanning),
                no_slash_prefix: true,
            },
        ];
        self.state.selected_suggestion = 0;
        self.state.awaiting_plan_decision = Some(PlanDecisionKind::Exit);
    }

    /// Re-inject plan mode suggestions (called when user presses Esc during plan decision).
    pub fn reinject_plan_suggestions(&mut self) {
        match &self.state.awaiting_plan_decision {
            Some(PlanDecisionKind::Enter) => self.show_enter_plan_suggestions(),
            Some(PlanDecisionKind::Exit) => self.show_exit_plan_suggestions(),
            None => {}
        }
    }

    /// Handle a plan mode action selected from the suggestion dropdown.
    pub fn handle_plan_mode_action(
        &mut self,
        action: PlanModeAction,
        event_tx: UnboundedSender<Event>,
    ) {
        // Clear suggestion UI state
        self.state.suggestions.clear();
        self.state.selected_suggestion = -1;
        let _decision_kind = self.state.awaiting_plan_decision.take();

        match action {
            PlanModeAction::ConfirmEnter => {
                // Reset exit flag — new plan mode session starts
                self.state.plan_mode_exit_handled = false;
                // Enable deferred mode so ExitPlanMode tool returns success as no-op
                strands_tools::utility::plan_state::set_deferred(false);
                // Enter plan mode now (we undid the tool's side-effect in the event handler).
                let _ = strands_tools::utility::plan_state::enter_plan_mode(None);
                strands_tools::utility::plan_state::set_deferred(true);
                self.state.permission_mode = PermissionMode::Plan;

                let plan_file = strands_tools::utility::plan_state::get_plan_file_path(None);
                self.state.pending_system_reminder = Some(
                    strands_tools::utility::plan_state::build_plan_mode_system_reminder(
                        &plan_file,
                    ),
                );

                let mut msg = ChatMessage::assistant_empty();
                msg.append_text("Entered plan mode. Exploring codebase and designing approach...");
                self.state.messages.push(msg);
                self.state.auto_scroll = true;
                self.state.scroll_offset = 0;

                // Resume agent with confirmation
                self.send_to_agent("Yes, enter plan mode. Continue with planning.".to_string(), event_tx);
            }
            PlanModeAction::RejectEnter => {
                // Disable deferred mode — not in plan mode anymore.
                strands_tools::utility::plan_state::set_deferred(false);
                // Plan mode was already undone in the event handler — just set UI mode.
                self.state.permission_mode = PermissionMode::Default;
                // Suppress future EnterPlanMode popups this session
                self.state.plan_mode_enter_rejected = true;

                let mut msg = ChatMessage::assistant_empty();
                msg.append_text("Skipping plan mode. Starting implementation...");
                self.state.messages.push(msg);
                self.state.auto_scroll = true;
                self.state.scroll_offset = 0;

                // Tell the model not to call EnterPlanMode again
                self.state.pending_system_reminder = Some(
                    "<system-reminder>\n\
                     The user declined plan mode. Do NOT call EnterPlanMode again.\n\
                     Proceed directly with implementation.\n\
                     </system-reminder>".to_string()
                );

                // Resume agent with rejection
                self.send_to_agent("No, don't enter plan mode. Start implementing directly.".to_string(), event_tx);
            }
            PlanModeAction::ApproveAcceptEdits => {
                self.state.plan_mode_exit_handled = true;
                self.complete_plan_exit(PermissionMode::AcceptEdits);
                self.send_to_agent("Implement the plan.".to_string(), event_tx);
            }
            PlanModeAction::ApproveManual => {
                self.state.plan_mode_exit_handled = true;
                self.complete_plan_exit(PermissionMode::Default);
                self.send_to_agent("Implement the plan.".to_string(), event_tx);
            }
            PlanModeAction::KeepPlanning => {
                // Reset exit flag — model will call ExitPlanMode again after revising
                self.state.plan_mode_exit_handled = false;
                // Deferred mode stays on — ExitPlanMode will remain a no-op for the next cycle.
                // Plan mode is still active (tool didn't mutate state).
                self.state.permission_mode = PermissionMode::Plan;

                // Clear conversation history — the old history has a successful
                // ExitPlanMode result which would confuse the model into thinking
                // it already exited. In TypeScript, the tool never ran so the model
                // receives an error tool_result. We simulate this by starting fresh.
                self.agent.clear_history();

                // Build full plan mode system-reminder (same as entering plan mode)
                let plan_file = strands_tools::utility::plan_state::get_plan_file_path(None);
                let plan_content = self.state.pending_plan_content.as_deref().unwrap_or("");
                let plan_reminder = strands_tools::utility::plan_state::build_plan_mode_system_reminder(&plan_file);
                self.state.pending_system_reminder = Some(format!(
                    "{}\n\n\
                     ## Previous Plan (User Wants Changes)\n\n\
                     The user rejected your previous plan below. Revise it based on their feedback.\n\
                     Update the plan file at {} and call ExitPlanMode when ready.\n\n\
                     Previous plan:\n{}\n",
                    plan_reminder,
                    plan_file.display(),
                    plan_content.trim()
                ));

                // Let the user type feedback — don't auto-submit
            }
        }
    }

    /// Complete plan exit: clear history, restore mode, set system-reminder.
    fn complete_plan_exit(&mut self, target_mode: PermissionMode) {
        let plan_file_path = self.state.pending_plan_file.take().unwrap_or_default();
        let plan_content = self.state.pending_plan_content.take().unwrap_or_default();

        // Disable deferred mode so exit_plan_mode actually mutates state.
        strands_tools::utility::plan_state::set_deferred(false);

        // Clear stale plan mode conversation context
        self.agent.clear_history();

        // Exit plan mode properly (restores previous permission mode)
        if strands_tools::utility::plan_state::is_in_plan_mode() {
            let _ = strands_tools::utility::plan_state::exit_plan_mode(None);
        }
        self.state.permission_mode = target_mode.clone();

        // Prepare implementation prompt for next turn
        if !plan_content.trim().is_empty() {
            self.state.pending_system_reminder = Some(format!(
                "<system-reminder>\n\
                 ## Exited Plan Mode\n\n\
                 You have exited plan mode. You can now make edits, run tools, and take actions.\n\
                 Permission mode: {}. {}\n\
                 The plan file is located at {} if you need to reference it.\n\
                 </system-reminder>\n\n\
                 Implement the following plan:\n\n{}",
                target_mode.label(),
                if target_mode == PermissionMode::AcceptEdits {
                    "File edits will be auto-accepted."
                } else {
                    "Each edit requires manual approval."
                },
                plan_file_path,
                plan_content.trim()
            ));
        } else {
            self.state.pending_system_reminder = Some(format!(
                "<system-reminder>\n\
                 ## Exited Plan Mode\n\n\
                 You have exited plan mode. You can now make edits, run tools, and take actions.\n\
                 The plan file is located at {} if you need to reference it.\n\
                 </system-reminder>",
                plan_file_path
            ));
        }
    }

    /// Send a message to the agent (extracted from submit() for reuse by plan mode actions).
    fn send_to_agent(&mut self, prompt: String, event_tx: UnboundedSender<Event>) {
        self.state.turn_count += 1;
        self.state.messages.push(ChatMessage::assistant_empty());
        self.state.agent_status = AgentStatus::Streaming;
        self.state.streaming_md_cache = None;
        self.state.auto_scroll = true;
        self.state.scroll_offset = 0;

        // Prepend any pending system-reminder
        let agent_prompt = if let Some(reminder) = self.state.pending_system_reminder.take() {
            format!("{}\n\n{}", reminder, prompt)
        } else {
            prompt
        };

        // Reset cancel signal and store agent clone for cancellation
        self.agent.reset_cancel();
        self.state.cancel_agent = Some(self.agent.clone());

        let agent = self.agent.clone();
        tokio::spawn(async move {
            Self::run_agent_stream(agent, &agent_prompt, event_tx).await;
        });
    }

    /// Resume a session by loading its messages and replacing current conversation.
    pub fn resume_session(
        &mut self,
        session_ref: String,
        event_tx: UnboundedSender<Event>,
    ) {
        self.state.messages.push(ChatMessage::user(format!("/resume {}", session_ref)));
        let mut msg = ChatMessage::assistant_empty();
        msg.append_text(&format!("Resuming session {}...", session_ref));
        self.state.messages.push(msg);
        self.state.auto_scroll = true;
        self.state.scroll_offset = 0;
        self.state.agent_status = AgentStatus::Streaming;

        let agent = self.agent.clone();
        let event_tx_clone = event_tx.clone();
        tokio::spawn(async move {
            let cwd = std::env::current_dir().unwrap_or_default();
            let sessions_dir = crate::session::SessionId::storage_dir(&cwd);
            match crate::session::resolve_and_load_full(&sessions_dir, &session_ref).await {
                Ok(resolved) => {
                    // Replace agent conversation with loaded messages.
                    // Use load_message (no hooks) to avoid re-writing them to the journal.
                    agent.clear_history();
                    for m in &resolved.messages {
                        agent.load_message(m.clone());
                    }
                    // Load the session title if available
                    if let Some(title) = resolved.title {
                        let _ = event_tx_clone.send(Event::SessionTitleLoaded(title));
                    }
                    // Send messages to TUI for display list reconstruction
                    let _ = event_tx_clone.send(Event::SessionResumed {
                        session_id: resolved.session_id.clone(),
                        messages: resolved.messages.clone(),
                    });
                    let _ = event_tx_clone.send(Event::AgentTextDelta(
                        format!("\nResumed session {} ({} messages)", resolved.session_id, resolved.messages.len()),
                    ));
                    let _ = event_tx_clone.send(Event::AgentDone);
                }
                Err(e) => {
                    let _ = event_tx_clone.send(Event::AgentError(
                        format!("Failed to resume: {}", e),
                    ));
                }
            }
        });
    }

    /// Trigger AI title generation from the conversation messages.
    /// Used by `/rename` with no args.
    fn trigger_ai_title_generation(&self, event_tx: UnboundedSender<Event>) {
        // Build conversation summary from recent messages
        let conversation_text: String = self
            .state
            .messages
            .iter()
            .filter(|m| !m.text_content().is_empty())
            .take(10)
            .map(|m| m.text_content())
            .collect::<Vec<_>>()
            .join("\n");

        if conversation_text.is_empty() {
            return;
        }

        let model = Arc::clone(&self.model);
        tokio::spawn(async move {
            if let Some(title) = crate::title_generator::generate_session_title(&conversation_text, model).await {
                let _ = event_tx.send(Event::AiTitleGenerated(title));
            }
        });
    }

    /// Run the agent stream, forwarding events to the TUI event channel.
    async fn run_agent_stream(
        agent: Agent,
        prompt: &str,
        event_tx: UnboundedSender<Event>,
    ) {
        match agent.stream_async(prompt).await {
            Ok(mut stream) => {
                while let Some(event) = stream.next().await {
                    match event {
                        Ok(ev) => {
                            let event_type = ev
                                .get("event_type")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");

                            let tui_event = match event_type {
                                "content_block_delta" => {
                                    ev.pointer("/delta/text")
                                        .and_then(|v| v.as_str())
                                        .map(|t| Event::AgentTextDelta(t.to_string()))
                                }
                                "tool_call" => {
                                    let name = ev
                                        .get("name")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("?")
                                        .to_string();
                                    let input = ev
                                        .get("input")
                                        .cloned()
                                        .unwrap_or(serde_json::Value::Null);
                                    let _ = event_tx.send(Event::AgentToolStart {
                                        name: name.clone(),
                                    });
                                    Some(Event::AgentToolCall { name, input })
                                }
                                "tool_result" => {
                                    let tool_name = ev.get("tool_name").and_then(|v| v.as_str()).unwrap_or("");
                                    let status = if let Some(s) = ev.get("status").and_then(|v| v.as_str()) {
                                        s.to_string()
                                    } else if ev.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false) {
                                        "error".to_string()
                                    } else {
                                        "success".to_string()
                                    };
                                    let content = ev
                                        .get("result_summary")
                                        .or_else(|| ev.get("content"))
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string();

                                    // EnterPlanMode: cancel agent and break immediately to stop
                                    // all further tool execution while awaiting user confirmation.
                                    if tool_name == "EnterPlanMode" && status == "success" {
                                        agent.cancel();
                                        let _ = event_tx.send(Event::EnterPlanModeRequested);
                                        break; // AgentDone sent after loop
                                    }

                                    // ExitPlanMode: cancel agent and break immediately to stop
                                    // all further tool execution while awaiting user approval.
                                    if tool_name == "ExitPlanMode" && status == "success" {
                                        agent.cancel();
                                        let plan_file = strands_tools::utility::plan_state::get_plan_file_path(None);
                                        let plan_content = std::fs::read_to_string(&plan_file).unwrap_or_default();
                                        let _ = event_tx.send(Event::PlanModeExitRequested {
                                            plan_content,
                                            plan_file: plan_file.display().to_string(),
                                        });
                                        break; // AgentDone sent after loop
                                    }

                                    Some(Event::AgentToolResult { status, content })
                                }
                                "message_stop" | "stream_complete" => {
                                    Some(Event::AgentDone)
                                }
                                "content_block_start" | "content_block_stop"
                                | "message_start" | "tool_execution_start"
                                | "tool_execution_progress"
                                | "tool_execution_complete" => None,
                                _ => {
                                    ev.get("data")
                                        .and_then(|d| d.as_str())
                                        .filter(|s| !s.is_empty() && s != &"complete")
                                        .map(|t| Event::AgentTextDelta(t.to_string()))
                                }
                            };

                            if let Some(e) = tui_event {
                                if event_tx.send(e).is_err() {
                                    break;
                                }
                            }
                        }
                        Err(e) => {
                            let _ = event_tx
                                .send(Event::AgentError(format!("{}", e)));
                            break;
                        }
                    }
                }
                let _ = event_tx.send(Event::AgentDone);
            }
            Err(e) => {
                let _ = event_tx.send(Event::AgentError(format!("{}", e)));
                let _ = event_tx.send(Event::AgentDone);
            }
        }
    }

    /// Generate rewind suggestions from conversation history.
    /// Each user message is a rewind target, shown most recent first.
    fn generate_rewind_suggestions(&self, query: &str) -> Vec<SuggestionItem> {
        let mut turn = 0usize;
        let mut items: Vec<SuggestionItem> = Vec::new();

        for (i, msg) in self.state.messages.iter().enumerate() {
            if matches!(msg.role, Role::User) {
                turn += 1;
                let text = msg.text_content();
                // Skip slash commands — not useful rewind targets
                if text.trim().starts_with('/') {
                    continue;
                }
                let message_id = format!("msg-{}", i);
                let has_changes = strands_tools::file::file_history::has_changes_since(&message_id);
                let file_tag = if has_changes { " [files changed]" } else { "" };

                let truncated = if text.len() > 60 {
                    format!("{}…", &text[..60])
                } else {
                    text.clone()
                };

                let name = format!("Turn {}", turn);
                let description = format!("{}{}", truncated, file_tag);

                // Filter by query
                if !query.is_empty()
                    && !name.to_lowercase().contains(query)
                    && !text.to_lowercase().contains(query)
                {
                    continue;
                }

                items.push(SuggestionItem {
                    name,
                    description,
                    model_id: None,
                    session_id: None,
                    rewind_info: Some((i, message_id)),
                    plan_mode_action: None,
                    no_slash_prefix: false,
                });
            }
        }

        // Most recent first
        items.reverse();
        items.truncate(10);
        items
    }

    /// Rewind conversation and/or files to a given message index.
    ///
    /// Called when the user selects a rewind suggestion from the `/rewind ` dropdown.
    /// `message_index` is the index into `self.state.messages` of the target user message.
    /// `message_id` is the file-history snapshot ID (e.g. "msg-5").
    pub fn rewind_to(&mut self, message_index: usize, message_id: &str) {
        let mut result_lines = Vec::new();

        // Count the turn number for display
        let turn_number = self.state.messages[..message_index]
            .iter()
            .filter(|m| matches!(m.role, Role::User))
            .count() + 1;

        // Restore files from snapshot
        match strands_tools::file::file_history::rewind_to_snapshot(message_id) {
            Ok(stats) => {
                if stats.files_changed > 0 {
                    result_lines.push(format!(
                        "Restored {} file(s) ({} restored, {} deleted)",
                        stats.files_changed, stats.files_restored, stats.files_deleted
                    ));
                }
            }
            Err(_) => {} // No snapshot or no changes — that's fine
        }

        // Truncate conversation to just before the selected message
        self.state.messages.truncate(message_index);
        self.state.message_cache.truncate(message_index);
        self.state.streaming_md_cache = None;

        // Truncate the agent's internal message history to match
        let sdk_msg_count = {
            let sdk_messages = self.agent.get_messages();
            let ui_user_count = self.state.messages.iter()
                .filter(|m| matches!(m.role, Role::User))
                .count();
            let mut sdk_user_count = 0;
            let mut target = sdk_messages.len();
            for (i, msg) in sdk_messages.iter().enumerate() {
                if msg.role == strands::types::content::Role::User {
                    sdk_user_count += 1;
                    if sdk_user_count > ui_user_count {
                        target = i;
                        break;
                    }
                }
            }
            target
        };
        self.agent.truncate_messages(sdk_msg_count);
        self.agent.reset_conversation_context();

        // Write a rewind marker to the JSONL journal so that resume
        // reconstructs the post-rewind conversation (fixes Bug B).
        if let Some(journal) = crate::session::get_journal() {
            let journal = std::sync::Arc::clone(journal);
            let target_count = sdk_msg_count;
            tokio::spawn(async move {
                // Load the journal to find the UUID of the target message
                let cwd = std::env::current_dir().unwrap_or_default();
                let sessions_dir = crate::session::SessionId::storage_dir(&cwd);
                let path = sessions_dir.join(format!("{}.jsonl", journal.session_id()));
                if let Ok(loaded) = strands::session::journal_session_manager::load_journal(&path).await {
                    // Find the UUID of the message at target_count - 1
                    // (0-indexed: the last message to keep)
                    let mut msg_idx = 0;
                    let mut target_uuid = None;
                    for (uuid, entry) in &loaded.messages {
                        if let strands::types::journal::JournalEntry::Message { .. } = entry {
                            msg_idx += 1;
                            if msg_idx == target_count {
                                target_uuid = Some(*uuid);
                                break;
                            }
                        }
                    }
                    if let Some(uuid) = target_uuid {
                        let _ = journal.set_rewind_marker(uuid).await;
                    }
                }
            });
        }

        result_lines.insert(0, format!("Rewound to Turn {}", turn_number));

        let mut msg = ChatMessage::assistant_empty();
        msg.append_text(&format!("[Rewind] {}", result_lines.join(". ")));
        self.state.messages.push(msg);
        self.state.auto_scroll = true;
        self.state.scroll_offset = 0;
    }

    pub fn handle_agent_event(&mut self, event: Event) {
        let last_msg = self.state.messages.last_mut();

        match event {
            Event::AgentTextDelta(text) => {
                if let Some(msg) = last_msg {
                    msg.append_text(&text);

                    // Track unseen messages when user scrolled away
                    if !self.state.auto_scroll {
                        self.state.unseen_message_count = self.state.unseen_message_count.saturating_add(0);
                        if self.state.unseen_from_line.is_none() {
                            self.state.unseen_from_line = Some(self.state.total_lines as usize);
                        }
                    }
                }
            }
            Event::AgentToolStart { name } => {
                if let Some(msg) = last_msg {
                    msg.add_tool_call(name, String::new(), ToolCallStatus::Running);

                }
            }
            Event::AgentToolCall { name, input } => {
                let summary = crate::repl::tool_call_summary(&name, &input);
                if let Some(msg) = last_msg {
                    for block in msg.blocks.iter_mut().rev() {
                        if let ContentBlock::ToolCall {
                            summary: ref mut s, ..
                        } = block
                        {
                            *s = summary;
                            break;
                        }
                    }

                }
            }
            Event::AgentToolResult { status, content } => {
                if let Some(msg) = last_msg {
                    let new_status = if status == "success" {
                        ToolCallStatus::Success
                    } else {
                        ToolCallStatus::Error
                    };
                    if new_status == ToolCallStatus::Error && !content.is_empty() {
                        for block in msg.blocks.iter_mut().rev() {
                            if let ContentBlock::ToolCall {
                                summary: ref mut s, ..
                            } = block
                            {
                                *s = content.clone();
                                break;
                            }
                        }
                    }
                    msg.set_last_tool_status(new_status);

                }
            }
            Event::EnterPlanModeRequested => {
                // In deferred mode, the tool returned success without mutating state.
                // No undo needed.

                if self.state.plan_mode_enter_rejected {
                    // User already rejected plan mode this session — don't ask again.
                    // (AgentDone will fire from the cancelled stream)
                    return;
                }

                // Model wants to enter plan mode — show inline suggestions for user to confirm/reject.
                self.state.agent_status = AgentStatus::Idle;
                self.state.cancel_agent = None;
                self.agent.reset_cancel();

                let mut msg = ChatMessage::assistant_empty();
                msg.append_text("Claude wants to enter **plan mode** to explore and design an implementation approach.\n\nIn plan mode, Claude will:\n- Explore the codebase thoroughly\n- Identify existing patterns\n- Design an implementation strategy\n- Present a plan for your approval\n\nNo code changes will be made until you approve the plan.");
                self.state.messages.push(msg);
                self.state.auto_scroll = true;
                self.state.scroll_offset = 0;

                self.show_enter_plan_suggestions();
            }
            Event::PlanModeExitRequested { plan_content, plan_file } => {
                if self.state.plan_mode_exit_handled {
                    // Already handled (duplicate event from cancelled stream) — ignore.
                    return;
                }

                // In deferred mode, the tool returned success + plan content without
                // mutating state. Plan mode is still active. No undo needed.

                // Model has finished planning — show plan + inline suggestions for approval.
                self.state.agent_status = AgentStatus::Idle;
                self.state.cancel_agent = None;
                self.agent.reset_cancel();

                // Show plan to user
                if !plan_content.trim().is_empty() {
                    let mut msg = ChatMessage::assistant_empty();
                    msg.append_text(&format!(
                        "**Ready to code?**\n\n**Plan** ({})\n\n{}\n\n---\n*Select an option below to approve or continue planning.*",
                        plan_file,
                        plan_content.trim()
                    ));
                    self.state.messages.push(msg);
                    self.state.auto_scroll = true;
                    self.state.scroll_offset = 0;
                }

                // Store plan for later use by complete_plan_exit
                self.state.pending_plan_content = Some(plan_content);
                self.state.pending_plan_file = Some(plan_file);

                self.show_exit_plan_suggestions();
            }
            Event::AgentDone => {
                self.state.agent_status = AgentStatus::Idle;
                self.state.cancel_agent = None;
                self.agent.reset_cancel();
                self.state.streaming_md_cache = None;
                // Handle /compact completion: replace agent history with summary
                if self.state.pending_compact {
                    self.state.pending_compact = false;
                    // Extract summary from the last assistant message
                    if let Some(msg) = self.state.messages.last() {
                        let summary_text = msg.text_content();
                        if !summary_text.is_empty() {
                            self.agent.replace_history_with_summary(&summary_text);
                        }
                    }
                }
                // Refresh context window tracking from agent
                self.state.context_percent_used = self.agent.context_percent_used();
                self.state.context_warning = self.agent.is_context_warning();
                self.state.context_critical = self.agent.is_context_critical();
                self.state.token_counts = self.agent.token_counts();
                // Bump unseen count for completed turn
                if !self.state.auto_scroll && self.state.unseen_from_line.is_some() {
                    self.state.unseen_message_count = self.state.unseen_message_count.saturating_add(1);
                }
            }
            Event::AgentError(e) => {
                // Detect context window overflow (413 / prompt_too_long)
                let is_context_full = e.contains("prompt_too_long")
                    || e.contains("413")
                    || e.contains("ContextWindowOverflow")
                    || e.contains("context window")
                    || e.contains("context_length_exceeded");

                if is_context_full {
                    self.state.agent_status = AgentStatus::Error(e.clone());
                    if let Some(msg) = last_msg {
                        msg.append_text("\n[Context window full — type /compact to summarize and continue]");
                    }
                } else {
                    self.state.agent_status = AgentStatus::Error(e.clone());
                    if let Some(msg) = last_msg {
                        msg.append_text(&format!("\n[Error: {}]", e));
                    }
                }
            }
            _ => {}
        }
    }
}
