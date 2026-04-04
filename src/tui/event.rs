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
    /// ExitPlanMode succeeded — clear conversation history (matches Claude Code's clear-context path).
    PlanModeExited,

    // MCP lifecycle
    /// MCP servers finished loading in the background.
    McpLoaded,

    // Session title events
    /// AI-generated session title received (from background model call).
    AiTitleGenerated(String),
    /// Session title loaded from disk (on resume).
    SessionTitleLoaded(String),

    // Session resume events
    /// Session resumed — carry SDK messages so the TUI can rebuild the display list.
    SessionResumed {
        session_id: String,
        messages: Vec<strands::types::content::Message>,
    },
}
