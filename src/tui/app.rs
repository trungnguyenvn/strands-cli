//! Application state and event dispatch for the TUI.

use futures::StreamExt;
use tokio::sync::mpsc::UnboundedSender;

use strands::Agent;

use super::event::Event;
use crate::commands::{
    self, CommandContext, CommandKind, CommandRegistry, CommandResult,
    DispatchResult, SuggestionItem,
};

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
}

impl AppState {
    pub fn new(model_name: String, command_registry: CommandRegistry) -> Self {
        let mut input = tui_textarea::TextArea::default();
        input.set_cursor_line_style(ratatui::style::Style::default());
        input.set_placeholder_text("Type a message... (Enter to send, Alt+Enter for newline)");
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
        }
    }
}

// ---------------------------------------------------------------------------
// TUI App
// ---------------------------------------------------------------------------

pub struct TuiApp {
    pub state: AppState,
    agent: Agent,
}

impl TuiApp {
    pub fn new(agent: Agent, model_name: String, command_registry: CommandRegistry) -> Self {
        Self {
            state: AppState::new(model_name, command_registry),
            agent,
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
                DispatchResult::Local(CommandResult::SwitchModel(model_id)) => {
                    self.state.messages.push(ChatMessage::user(trimmed.to_string()));
                    let mut msg = ChatMessage::assistant_empty();
                    msg.append_text(&format!("Switching model to {}...", model_id));
                    self.state.messages.push(msg);
                    self.state.auto_scroll = true;
                    self.state.scroll_offset = 0;
                    self.reset_input();

                    // Build new model and swap (async)
                    let agent = self.agent.clone();
                    let event_tx_clone = event_tx.clone();
                    let model_id_clone = model_id.clone();
                    let new_model_name = model_id.clone();
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
                    self.state.model_name = new_model_name;
                    self.state.agent_status = AgentStatus::Streaming;
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
    fn reset_input(&mut self) {
        self.state.input = tui_textarea::TextArea::default();
        self.state.input.set_cursor_line_style(ratatui::style::Style::default());
        self.state.input.set_placeholder_text("Type a message... (Enter to send, Alt+Enter for newline)");
        self.state.suggestions.clear();
        self.state.selected_suggestion = -1;
    }

    /// Update autocomplete suggestions based on current input.
    /// Called after every keystroke. Mirrors Claude Code's `updateSuggestions`.
    pub fn update_suggestions(&mut self) {
        let text = self.state.input.lines().join("\n");
        let new_suggestions =
            commands::generate_suggestions(&text, &self.state.command_registry);

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
    /// Mirrors Claude Code's `applyCommandSuggestion`.
    pub fn accept_suggestion(&mut self) {
        if self.state.selected_suggestion < 0
            || self.state.selected_suggestion as usize >= self.state.suggestions.len()
        {
            return;
        }
        let name = self.state.suggestions[self.state.selected_suggestion as usize]
            .name
            .clone();
        let new_input = format!("/{} ", name);

        // Replace textarea content
        self.state.input = tui_textarea::TextArea::default();
        self.state.input.set_cursor_line_style(ratatui::style::Style::default());
        self.state.input.set_placeholder_text("Type a message... (Enter to send, Alt+Enter for newline)");
        for ch in new_input.chars() {
            self.state.input.insert_char(ch);
        }

        // Clear suggestions after accepting
        self.state.suggestions.clear();
        self.state.selected_suggestion = -1;
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
