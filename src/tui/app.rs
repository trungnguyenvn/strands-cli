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
    },
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
        self.blocks.push(ContentBlock::ToolCall {
            name,
            summary,
            status,
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
    pub total_lines: u16, // cached for scroll math
}

impl AppState {
    pub fn new(model_name: String) -> Self {
        let mut input = tui_textarea::TextArea::default();
        input.set_cursor_line_style(ratatui::style::Style::default());
        input.set_placeholder_text("Type a message... (Enter to send, Alt+Enter for newline)");
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
                                    "content_block_start" => {
                                        ev.pointer("/content_block/name")
                                            .and_then(|v| v.as_str())
                                            .map(|n| Event::AgentToolStart {
                                                name: n.to_string(),
                                            })
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
                                        Some(Event::AgentToolCall { name, input })
                                    }
                                    "tool_result" => {
                                        let status = ev
                                            .get("status")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("?")
                                            .to_string();
                                        let content = ev
                                            .get("content")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string();
                                        Some(Event::AgentToolResult { status, content })
                                    }
                                    "message_stop" => Some(Event::AgentDone),
                                    _ => {
                                        // Handle simple "data" field streaming
                                        ev.get("data")
                                            .and_then(|d| d.as_str())
                                            .filter(|s| !s.is_empty())
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
            Event::AgentToolResult { status, .. } => {
                if let Some(msg) = last_msg {
                    let new_status = if status == "success" {
                        ToolCallStatus::Success
                    } else {
                        ToolCallStatus::Error
                    };
                    msg.set_last_tool_status(new_status);
                }
            }
            Event::AgentDone => {
                self.state.agent_status = AgentStatus::Idle;
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
