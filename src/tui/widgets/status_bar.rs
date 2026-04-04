//! Status bar widget — model name, contextual hints, turn count, scroll position.

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::tui::app::{AgentStatus, AppState, McpStatus};

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn render_status_bar(state: &AppState, frame: &mut Frame, area: Rect) {
    let sep = Span::styled(" │ ", Style::default().fg(Color::DarkGray));

    // Left side: contextual hint (like Claude Code's PromptInputFooterLeftSide)
    let hint = match &state.agent_status {
        AgentStatus::Streaming => Span::styled(
            " esc to interrupt",
            Style::default().fg(Color::DarkGray),
        ),
        AgentStatus::Error(_) => Span::styled(
            " enter to retry",
            Style::default().fg(Color::DarkGray),
        ),
        AgentStatus::Idle => Span::styled(
            " ? for shortcuts",
            Style::default().fg(Color::DarkGray),
        ),
    };

    let mcp_span = match &state.mcp_status {
        McpStatus::Loading => {
            let frame_char = SPINNER_FRAMES[state.tick_count % SPINNER_FRAMES.len()];
            Span::styled(
                format!("{} MCP connecting…", frame_char),
                Style::default().fg(Color::Yellow),
            )
        }
        McpStatus::Warning { failed, .. } => Span::styled(
            format!("MCP: {} failed", failed),
            Style::default().fg(Color::Red),
        ),
        McpStatus::None => Span::raw(""),
    };

    let turn_info = if state.turn_count > 0 {
        Span::styled(
            format!("{} turns", state.turn_count),
            Style::default().fg(Color::DarkGray),
        )
    } else {
        Span::raw("")
    };

    let scroll_info = if !state.auto_scroll && state.total_lines > 0 {
        let pct = 100u16.saturating_sub(
            (state.scroll_offset as u32 * 100 / state.total_lines.max(1) as u32) as u16,
        )
        .min(100);
        Span::styled(
            format!("↑ {}%", pct),
            Style::default().fg(Color::Yellow),
        )
    } else {
        Span::raw("")
    };

    let model = Span::styled(
        format!(" {}", state.model_name),
        Style::default().fg(Color::DarkGray),
    );

    // Build spans: hint | model | mcp? | turns? | scroll?
    let mut spans = vec![hint];

    spans.push(sep.clone());
    spans.push(model);

    if !matches!(state.mcp_status, McpStatus::None) {
        spans.push(sep.clone());
        spans.push(mcp_span);
    }

    if state.turn_count > 0 {
        spans.push(sep.clone());
        spans.push(turn_info);
    }

    if !state.auto_scroll && state.total_lines > 0 {
        spans.push(sep);
        spans.push(scroll_info);
    }

    let line = Line::from(spans);
    let bar = Paragraph::new(line).style(Style::default().bg(Color::Rgb(30, 30, 30)));
    frame.render_widget(bar, area);
}
