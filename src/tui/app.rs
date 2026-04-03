//! Application state and event dispatch for the TUI.

use futures::StreamExt;
use tokio::sync::mpsc::UnboundedSender;

use strands::Agent;

use super::event::Event;

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
}

impl AppState {
    pub fn new(model_name: String) -> Self {
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
    pub fn new(agent: Agent, model_name: String) -> Self {
        Self {
            state: AppState::new(model_name),
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

        // Handle slash commands
        let trimmed = prompt.trim();
        if trimmed == "/exit" || trimmed == "/quit" {
            self.state.should_quit = true;
            return;
        }
        if trimmed == "/clear" {
            self.state.messages.clear();
            self.agent.clear_history();
            // Reset input
            self.state.input = tui_textarea::TextArea::default();
            self.state.input.set_cursor_line_style(ratatui::style::Style::default());
            self.state.input.set_placeholder_text("Type a message... (Enter to send, Alt+Enter for newline)");
            return;
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

        // Reset input
        self.state.input = tui_textarea::TextArea::default();
        self.state.input.set_cursor_line_style(ratatui::style::Style::default());
        self.state.input.set_placeholder_text("Type a message... (Enter to send, Alt+Enter for newline)");

        // Reset cancel signal and store agent clone for cancellation
        self.agent.reset_cancel();
        self.state.cancel_agent = Some(self.agent.clone());

        // Spawn agent streaming task
        let agent = self.agent.clone();
        tokio::spawn(async move {
            match agent.stream_async(&prompt).await {
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
                                        // tool_call carries the tool name — emit
                                        // ToolStart first, then ToolCall with input
                                        let name = ev
                                            .get("name")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("?")
                                            .to_string();
                                        let input = ev
                                            .get("input")
                                            .cloned()
                                            .unwrap_or(serde_json::Value::Null);
                                        // Send ToolStart before ToolCall
                                        let _ = event_tx.send(Event::AgentToolStart {
                                            name: name.clone(),
                                        });
                                        Some(Event::AgentToolCall { name, input })
                                    }
                                    "tool_result" => {
                                        // Two formats: Anthropic (status + result_summary)
                                        // and Bedrock (is_error + content)
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
                                    // Ignore known non-content events
                                    "content_block_start" | "content_block_stop"
                                    | "message_start" | "tool_execution_start"
                                    | "tool_execution_progress"
                                    | "tool_execution_complete" => None,
                                    _ => {
                                        // Handle simple "data" field streaming
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
                    // Ensure Done is sent even if stream ends without message_stop
                    let _ = event_tx.send(Event::AgentDone);
                }
                Err(e) => {
                    let _ = event_tx.send(Event::AgentError(format!("{}", e)));
                    let _ = event_tx.send(Event::AgentDone);
                }
            }
        });
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
