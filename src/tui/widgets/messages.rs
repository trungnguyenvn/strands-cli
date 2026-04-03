//! Message history widget — renders all chat messages as scrollable content.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap};
use ratatui::Frame;

use crate::tui::app::{AppState, ChatMessage, ContentBlock, Role};
use crate::tui::widgets::markdown::markdown_to_lines;
use crate::tui::widgets::tool_call::render_tool_call;

pub fn render_messages(state: &mut AppState, frame: &mut Frame, area: Rect) {
    let mut all_lines: Vec<Line<'static>> = Vec::new();

    for msg in &state.messages {
        // Blank line between messages
        if !all_lines.is_empty() {
            all_lines.push(Line::from(""));
        }
        all_lines.extend(render_message(msg, state.tick_count));
    }

    let total_lines = all_lines.len() as u16;
    state.total_lines = total_lines;
    let visible = area.height;

    // scroll_offset=0 means pinned to bottom
    let max_offset = total_lines.saturating_sub(visible);
    let y_scroll = if state.auto_scroll {
        max_offset
    } else {
        max_offset.saturating_sub(state.scroll_offset)
    };

    let paragraph = Paragraph::new(all_lines)
        .wrap(Wrap { trim: false })
        .scroll((y_scroll, 0));

    frame.render_widget(paragraph, area);

    // Scrollbar
    if total_lines > visible {
        let mut scrollbar_state = ScrollbarState::new(max_offset as usize)
            .position(y_scroll as usize);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight),
            area,
            &mut scrollbar_state,
        );
    }
}

fn render_message(msg: &ChatMessage, tick: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    match msg.role {
        Role::User => {
            for block in &msg.blocks {
                if let ContentBlock::Text(text) = block {
                    for line in text.lines() {
                        lines.push(Line::from(vec![
                            Span::styled(
                                "> ",
                                Style::default()
                                    .fg(Color::Green)
                                    .add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(
                                line.to_string(),
                                Style::default().add_modifier(Modifier::BOLD),
                            ),
                        ]));
                    }
                }
            }
        }
        Role::Assistant => {
            for block in &msg.blocks {
                match block {
                    ContentBlock::Text(text) => {
                        if !text.is_empty() {
                            lines.extend(markdown_to_lines(text));
                        }
                    }
                    ContentBlock::ToolCall {
                        name,
                        summary,
                        status,
                    } => {
                        lines.push(render_tool_call(name, summary, status, tick));
                    }
                }
            }
        }
    }

    lines
}
