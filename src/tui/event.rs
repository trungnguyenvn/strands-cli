//! Unified event type for the TUI — merges terminal events with agent stream events.

#![allow(dead_code)]

use crossterm::event::{KeyEvent, MouseEvent};

#[derive(Clone, Debug)]
pub enum Event {
    // Terminal events
    Key(KeyEvent),
    Mouse(MouseEvent),
    Resize(u16, u16),
    Paste(String),
    FocusGained,
    FocusLost,

    // Timer events
    Tick,
    Render,

    // Agent stream events
    AgentTextDelta(String),
    AgentToolStart { name: String },
    AgentToolCall { name: String, input: serde_json::Value },
    AgentToolResult { status: String, content: String },
    AgentDone,
    AgentError(String),

    // MCP lifecycle
    /// MCP servers finished loading in the background.
    McpLoaded,
}
