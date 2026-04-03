//! Status bar widget — model name, status, turn count, scroll position, key hints.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::tui::app::{AgentStatus, AppState};

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn render_status_bar(state: &AppState, frame: &mut Frame, area: Rect) {
    let spinner = match &state.agent_status {
        AgentStatus::Streaming => {
            let frame_char = SPINNER_FRAMES[state.tick_count % SPINNER_FRAMES.len()];
            Span::styled(
                format!(" {} ", frame_char),
                Style::default().fg(Color::Cyan),
            )
        }
        _ => Span::raw("   "),
    };

    let status_text = match &state.agent_status {
        AgentStatus::Idle => Span::styled("ready", Style::default().fg(Color::DarkGray)),
        AgentStatus::Streaming => Span::styled(
            "streaming...",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        AgentStatus::Error(e) => {
            let msg = if e.len() > 40 {
                format!("{}...", &e[..40])
            } else {
                e.clone()
            };
            Span::styled(msg, Style::default().fg(Color::Red))
        }
    };

    let turn_info = Span::styled(
        format!(" {} turns ", state.turn_count),
        Style::default().fg(Color::DarkGray),
    );

    let scroll_info = if state.auto_scroll || state.total_lines == 0 {
        Span::styled(" \u{2193} bottom ", Style::default().fg(Color::DarkGray))
    } else {
        let pct = 100u16.saturating_sub(
            (state.scroll_offset as u32 * 100 / state.total_lines.max(1) as u32) as u16,
        )
        .min(100);
        Span::styled(
            format!(" \u{2191} {}% ", pct),
            Style::default().fg(Color::Yellow),
        )
    };

    let key_hints = match &state.agent_status {
        AgentStatus::Streaming => Span::styled(
            " Ctrl+C/Esc cancel ",
            Style::default().fg(Color::DarkGray),
        ),
        _ => Span::styled(
            " pgup/pgdn scroll \u{2502} /clear \u{2502} /exit ",
            Style::default().fg(Color::DarkGray),
        ),
    };

    let sep = Span::styled(" \u{2502} ", Style::default().fg(Color::DarkGray));

    let line = Line::from(vec![
        Span::styled(
            format!(" {} ", state.model_name),
            Style::default().fg(Color::DarkGray),
        ),
        sep.clone(),
        spinner,
        status_text,
        sep.clone(),
        turn_info,
        sep.clone(),
        scroll_info,
        sep,
        key_hints,
    ]);

    let bar = Paragraph::new(line).style(Style::default().bg(Color::Rgb(30, 30, 30)));
    frame.render_widget(bar, area);
}
