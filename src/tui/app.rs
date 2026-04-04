//! Application state and event dispatch for the TUI.

use futures::StreamExt;
use tokio::sync::mpsc::UnboundedSender;

use strands::Agent;

use super::event::Event;
use crate::commands::{
    self, CommandContext, CommandKind, CommandRegistry, CommandResult,
    DispatchResult, SuggestionItem,
};
use crate::mcp::McpSession;

// ---------------------------------------------------------------------------
// State types
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub enum AgentStatus {
    Idle,
    Streaming,
    Error(String),
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
}

impl AppState {
    pub fn new(model_name: String, command_registry: CommandRegistry, mcp_servers: Vec<crate::mcp::McpServerInfo>) -> Self {
        let mut input = tui_textarea::TextArea::default();
        input.set_cursor_line_style(ratatui::style::Style::default());
        input.set_placeholder_text(" ");
        let terminal_width = crossterm::terminal::size().map(|(w, _)| w).unwrap_or(80);
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
        }
    }
}

// ---------------------------------------------------------------------------
// TUI App
// ---------------------------------------------------------------------------

pub struct TuiApp {
    pub state: AppState,
    agent: Agent,
    /// MCP clients kept alive for session duration (Drop kills subprocesses / closes HTTP sessions).
    #[allow(dead_code)]
    _mcp_clients: Option<(Vec<strands::tools::mcp::MCPClient>, Vec<strands::tools::mcp::MCPHttpClient>)>,
}

impl TuiApp {
    pub fn new(agent: Agent, model_name: String, command_registry: CommandRegistry) -> Self {
        Self {
            state: AppState::new(model_name, command_registry, Vec::new()),
            agent,
            _mcp_clients: None,
        }
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
            let ctx = CommandContext {
                model_name: self.state.model_name.clone(),
                turn_count: self.state.turn_count,
                message_count: self.state.messages.len(),
                all_commands: self.state.command_registry.command_infos(),
                mcp_servers: self.state.mcp_servers.clone(),
            };
            match commands::dispatch(trimmed, &self.state.command_registry, &ctx) {
                DispatchResult::Local(CommandResult::Quit) => {
                    self.state.should_quit = true;
                    return;
                }
                DispatchResult::Local(CommandResult::Clear) => {
                    self.state.messages.clear();
                    self.agent.clear_history();
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
                DispatchResult::Local(CommandResult::SwitchModel(model_id)) => {
                    self.reset_input();
                    self.switch_model(model_id, event_tx);
                    return;
                }
                DispatchResult::Prompt(expanded) => {
                    // Show the original command as user message, send expanded prompt to model
                    self.state.messages.push(ChatMessage::user(trimmed.to_string()));
                    self.state.messages.push(ChatMessage::assistant_empty());
                    self.state.agent_status = AgentStatus::Streaming;
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

        // Add user message
        self.state.messages.push(ChatMessage::user(prompt.clone()));
        self.state.messages.push(ChatMessage::assistant_empty());
        self.state.agent_status = AgentStatus::Streaming;
        self.state.auto_scroll = true;
        self.state.scroll_offset = 0;

        self.reset_input();

        // Reset cancel signal and store agent clone for cancellation
        self.agent.reset_cancel();
        self.state.cancel_agent = Some(self.agent.clone());

        // Spawn agent streaming task
        let agent = self.agent.clone();
        tokio::spawn(async move {
            Self::run_agent_stream(agent, &prompt, event_tx).await;
        });
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

        let ctx = CommandContext {
            model_name: self.state.model_name.clone(),
            turn_count: self.state.turn_count,
            message_count: self.state.messages.len(),
            all_commands: self.state.command_registry.command_infos(),
            mcp_servers: self.state.mcp_servers.clone(),
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
                self.agent.clear_history();
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
            _ => {} // Non-local or prompt commands are not allowed during streaming
        }
    }

    /// Reset the input textarea to its default state.
    /// Switch the model — shows a message and spawns async build+swap.
    /// Used by both `/model <alias>` dispatch and the interactive picker.
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
    fn set_input(&mut self, text: &str) {
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
        let new_suggestions =
            commands::generate_suggestions(&text, &self.state.command_registry, &self.state.model_name);

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

    /// Handle an agent stream event — mutate state accordingly.
    pub fn handle_agent_event(&mut self, event: Event) {
        let last_msg = self.state.messages.last_mut();

        match event {
            Event::AgentTextDelta(text) => {
                if let Some(msg) = last_msg {
                    msg.append_text(&text);
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
                    // Update the last tool call block with the summary
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
                    // For errors, show the error reason in the summary
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
            Event::AgentDone => {
                self.state.agent_status = AgentStatus::Idle;
                self.state.cancel_agent = None;
                self.agent.reset_cancel();
            }
            Event::AgentError(e) => {
                self.state.agent_status = AgentStatus::Error(e.clone());
                if let Some(msg) = last_msg {
                    msg.append_text(&format!("\n[Error: {}]", e));
                }
            }
            _ => {}
        }
    }
}
