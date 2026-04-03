//! Status bar widget — model name, status, key hints.

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

    let msg_count = state.messages.len();
    let msg_info = Span::styled(
        format!(" {} msgs", msg_count / 2), // user+assistant pairs
        Style::default().fg(Color::DarkGray),
    );

    let key_hints = match &state.agent_status {
        AgentStatus::Streaming => {
            Span::styled(" esc stop ", Style::default().fg(Color::DarkGray))
        }
        _ => Span::styled(
            " pgup/pgdn scroll │ /clear │ /exit ",
            Style::default().fg(Color::DarkGray),
        ),
    };

    let separator = Span::styled(" │ ", Style::default().fg(Color::DarkGray));

    let line = Line::from(vec![
        Span::styled(
            format!(" {} ", state.model_name),
            Style::default().fg(Color::DarkGray),
        ),
        separator.clone(),
        spinner,
        status_text,
        separator.clone(),
        msg_info,
        separator,
        key_hints,
    ]);

    let bar = Paragraph::new(line).style(Style::default().bg(Color::Rgb(30, 30, 30)));
    frame.render_widget(bar, area);
}
