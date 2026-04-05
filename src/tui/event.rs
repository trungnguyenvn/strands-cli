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
    AgentReasoningDelta(String),
    AgentToolStart { name: String, tool_use_id: String },
    AgentToolCall { name: String, input: serde_json::Value, tool_use_id: String },
    AgentToolResult { status: String, content: String, tool_use_id: String },
    AgentDone,
    AgentError(String),
    /// EnterPlanMode tool succeeded — show inline suggestions for user to confirm/reject.
    EnterPlanModeRequested,
    /// ExitPlanMode tool succeeded — show plan + inline suggestions for user approval.
    PlanModeExitRequested {
        plan_content: String,
        plan_file: String,
    },

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
