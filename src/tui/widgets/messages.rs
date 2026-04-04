//! Message history widget — renders all chat messages as scrollable content.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap};
use ratatui::Frame;

use crate::tui::app::{AppState, ChatMessage, ContentBlock, Role, ToolCallStatus};
use crate::tui::widgets::markdown::markdown_to_lines;
use crate::tui::widgets::tool_call::{render_tool_call, render_tool_call_group};

pub fn render_messages(state: &mut AppState, frame: &mut Frame, area: Rect) {
    // Store the messages area for selection coordinate mapping
    state.selection.messages_area = area;

    // Welcome header when no messages
    if state.messages.is_empty() {
        let welcome = vec![
            Line::from(""),
            Line::from(vec![
                Span::styled(
                    "  Strands",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    " CLI",
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("  Model: ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    state.model_name.clone(),
                    Style::default().fg(Color::White),
                ),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "  Type a message and press Enter to start.",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(Span::styled(
                "  /help for commands \u{2502} /clear to reset \u{2502} /exit to quit",
                Style::default().fg(Color::DarkGray),
            )),
        ];
        state.selection.rendered_lines = welcome.iter().map(line_to_plain_text).collect();
        state.selection.rendered_y_scroll = 0;
        let para = Paragraph::new(welcome);
        frame.render_widget(para, area);
        return;
    }

    let mut all_lines: Vec<Line<'static>> = Vec::new();

    for msg in &state.messages {
        if !all_lines.is_empty() {
            all_lines.push(Line::from(""));
            all_lines.push(Line::from(""));
        }
        all_lines.extend(render_message(msg, state.tick_count, state.terminal_width));
    }

    // Bottom padding so last message doesn't touch the input box
    if !all_lines.is_empty() {
        all_lines.push(Line::from(""));
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

    // Store rendered text lines and scroll offset for selection
    state.selection.rendered_lines = all_lines.iter().map(line_to_plain_text).collect();
    state.selection.rendered_y_scroll = y_scroll;

    let paragraph = Paragraph::new(all_lines)
        .wrap(Wrap { trim: false })
        .scroll((y_scroll, 0));

    frame.render_widget(paragraph, area);

    // Render selection highlight overlay
    render_selection_highlight(state, frame, area, y_scroll);

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

/// Convert a ratatui Line to plain text for selection extraction.
fn line_to_plain_text(line: &Line<'_>) -> String {
    let mut s = String::new();
    for span in line.spans.iter() {
        s.push_str(span.content.as_ref());
    }
    s
}

/// Render the selection highlight as an overlay on the messages area.
fn render_selection_highlight(state: &AppState, frame: &mut Frame, area: Rect, _y_scroll: u16) {
    let sel = &state.selection;
    if sel.anchor == sel.end && !sel.active {
        return;
    }

    let ((sr, sc), (er, ec)) = sel.ordered();

    // Only highlight within the messages area
    for row in sr..=er {
        if row < area.y || row >= area.y + area.height {
            continue;
        }

        let col_start = if row == sr { sc } else { area.x };
        let col_end = if row == er {
            (ec + 1).min(area.x + area.width)
        } else {
            area.x + area.width
        };

        if col_start >= col_end {
            continue;
        }

        // Read existing cells and re-render with inverted style
        let buf = frame.buffer_mut();
        for col in col_start..col_end {
            if let Some(cell) = buf.cell_mut(ratatui::layout::Position::new(col, row)) {
                // Swap fg/bg for selection highlight
                let fg = cell.fg;
                let bg = cell.bg;
                cell.set_fg(if bg == Color::Reset { Color::Black } else { bg });
                cell.set_bg(if fg == Color::Reset { Color::White } else { fg });
            }
        }
    }
}

/// Left margin prefix for all message content.
const MARGIN: &str = "  ";

fn render_message(msg: &ChatMessage, tick: usize, max_width: u16) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let margin = Span::raw(MARGIN);

    match msg.role {
        Role::User => {
            for block in &msg.blocks {
                if let ContentBlock::Text(text) = block {
                    for line in text.lines() {
                        lines.push(Line::from(vec![
                            margin.clone(),
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
            let blocks = &msg.blocks;
            let mut i = 0;
            while i < blocks.len() {
                match &blocks[i] {
                    ContentBlock::Text(text) => {
                        if !text.is_empty() {
                            for mut line in markdown_to_lines(text, max_width.saturating_sub(MARGIN.len() as u16)) {
                                line.spans.insert(0, margin.clone());
                                lines.push(line);
                            }
                        }
                        i += 1;
                    }
                    ContentBlock::ToolCall {
                        name,
                        summary,
                        status,
                        group_key,
                    } => {
                        // Try to collapse consecutive same-group successful tool calls
                        if let Some(gk) = group_key {
                            if *status == ToolCallStatus::Success {
                                let mut run: Vec<(&str, &str)> =
                                    vec![(name.as_str(), summary.as_str())];
                                let mut j = i + 1;
                                while j < blocks.len() {
                                    if let ContentBlock::ToolCall {
                                        name: n2,
                                        summary: s2,
                                        status: st2,
                                        group_key: Some(gk2),
                                    } = &blocks[j]
                                    {
                                        if gk2 == gk && *st2 == ToolCallStatus::Success {
                                            run.push((n2.as_str(), s2.as_str()));
                                            j += 1;
                                            continue;
                                        }
                                    }
                                    break;
                                }
                                if run.len() > 1 {
                                    let mut line = render_tool_call_group(&run);
                                    line.spans.insert(0, margin.clone());
                                    lines.push(line);
                                    i = j;
                                    continue;
                                }
                            }
                        }
                        let mut line = render_tool_call(name, summary, status, tick);
                        line.spans.insert(0, margin.clone());
                        lines.push(line);
                        i += 1;
                    }
                }
            }
        }
    }

    lines
}
