//! Status bar widget — model name, contextual hints, turn count, scroll position.

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::tui::app::{AgentStatus, AppState, McpStatus, PermissionMode, VimMode};

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn render_status_bar(state: &AppState, frame: &mut Frame, area: Rect) {
    let sep = Span::styled(" │ ", Style::default().fg(Color::DarkGray));

    // Left side: contextual hint (like Claude Code's PromptInputFooterLeftSide)
    // Show "press Ctrl+C again to quit" when within the double-tap window
    let ctrl_c_pending = state.last_ctrl_c_tick.map_or(false, |t| {
        state.tick_count.wrapping_sub(t) <= 24 // ~2 seconds at 12 Hz tick rate
    }) && !matches!(state.agent_status, AgentStatus::Streaming);

    let hint = if ctrl_c_pending {
        Span::styled(
            " Press Ctrl+C again to quit",
            Style::default().fg(Color::Yellow),
        )
    } else {
        match &state.agent_status {
            AgentStatus::Streaming => Span::styled(
                " esc to interrupt",
                Style::default().fg(Color::DarkGray),
            ),
            AgentStatus::Error(ref e) if e.contains("context") || e.contains("413") || e.contains("prompt_too_long") => Span::styled(
                " /compact to continue",
                Style::default().fg(Color::Red),
            ),
            AgentStatus::Error(_) => Span::styled(
                " enter to retry",
                Style::default().fg(Color::DarkGray),
            ),
            AgentStatus::Idle => Span::styled(
                " ? for shortcuts",
                Style::default().fg(Color::DarkGray),
            ),
        }
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

    // Permission mode badge (non-default modes shown prominently)
    let mode_span = if state.permission_mode != PermissionMode::Default {
        Span::styled(
            format!(" {} ", state.permission_mode.label()),
            Style::default()
                .fg(Color::Black)
                .bg(state.permission_mode.color())
                .add_modifier(ratatui::style::Modifier::BOLD),
        )
    } else {
        Span::raw("")
    };

    // Session ID (abbreviated to last 8 chars, like Claude Code's display)
    let session_span = if let Some(ref id) = state.session_id {
        let abbrev = if id.len() > 8 { &id[id.len() - 8..] } else { id };
        Span::styled(
            format!("#{}", abbrev),
            Style::default().fg(Color::DarkGray),
        )
    } else {
        Span::raw("")
    };

    // Build spans: hint | mode? | model | session? | mcp? | turns? | scroll?
    let mut spans = vec![hint];

    if state.permission_mode != PermissionMode::Default {
        spans.push(sep.clone());
        spans.push(mode_span);
    }

    spans.push(sep.clone());
    spans.push(model);

    if state.session_id.is_some() {
        spans.push(sep.clone());
        spans.push(session_span);
    }

    if !matches!(state.mcp_status, McpStatus::None) {
        spans.push(sep.clone());
        spans.push(mcp_span);
    }

    if state.turn_count > 0 {
        spans.push(sep.clone());
        spans.push(turn_info);
    }

    // Context window usage indicator (matching Claude Code)
    if let Some(pct) = state.context_percent_used {
        if pct > 50.0 {
            let (color, label) = if state.context_critical {
                (Color::Red, format!("ctx {:.0}%", pct))
            } else if state.context_warning {
                (Color::Yellow, format!("ctx {:.0}%", pct))
            } else {
                (Color::DarkGray, format!("ctx {:.0}%", pct))
            };
            spans.push(sep.clone());
            spans.push(Span::styled(label, Style::default().fg(color)));
        }
    }

    if !state.auto_scroll && state.total_lines > 0 {
        spans.push(sep.clone());
        spans.push(scroll_info);
    }

    // Unseen messages indicator
    if state.unseen_message_count > 0 && !state.auto_scroll {
        spans.push(sep.clone());
        spans.push(Span::styled(
            format!("{} new ↓", state.unseen_message_count),
            Style::default().fg(Color::Yellow),
        ));
    }

    // Vim mode indicator
    match state.vim_mode {
        VimMode::Normal => {
            spans.push(sep.clone());
            spans.push(Span::styled(
                "NORMAL",
                Style::default().fg(Color::Cyan).add_modifier(ratatui::style::Modifier::BOLD),
            ));
        }
        VimMode::Insert => {
            spans.push(sep);
            spans.push(Span::styled(
                "INSERT",
                Style::default().fg(Color::Green).add_modifier(ratatui::style::Modifier::BOLD),
            ));
        }
        VimMode::Off => {}
    }

    let line = Line::from(spans);
    let bar = Paragraph::new(line).style(Style::default().bg(Color::Rgb(30, 30, 30)));
    frame.render_widget(bar, area);
}
