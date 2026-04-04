//! Permission request overlay — mirrors Claude Code's PermissionRequest component.
//!
//! Renders a centered overlay asking the user to allow or deny a tool call.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::tui::app::PermissionRequest;

/// Render the permission request overlay centered on screen.
pub fn render_permission_overlay(request: &PermissionRequest, frame: &mut Frame, area: Rect) {
    // Size the overlay: ~50% width, 8 lines tall
    let overlay_width = (area.width * 3 / 5).max(40).min(area.width);
    let overlay_height = 8u16.min(area.height);

    let x = area.x + (area.width.saturating_sub(overlay_width)) / 2;
    let y = area.y + (area.height.saturating_sub(overlay_height)) / 2;
    let overlay_area = Rect::new(x, y, overlay_width, overlay_height);

    // Clear background
    frame.render_widget(Clear, overlay_area);

    let block = Block::default()
        .title(" Permission Required ")
        .title_style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow));

    let inner = block.inner(overlay_area);
    frame.render_widget(block, overlay_area);

    // Content
    let chunks = Layout::vertical([
        Constraint::Length(1), // tool name
        Constraint::Length(1), // blank
        Constraint::Length(1), // summary
        Constraint::Fill(1),  // spacer
        Constraint::Length(1), // key hints
    ])
    .split(inner);

    // Tool name
    let tool_line = Line::from(vec![
        Span::styled("  Tool: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            request.tool_name.clone(),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
    ]);
    frame.render_widget(Paragraph::new(tool_line), chunks[0]);

    // Summary (truncated to fit)
    let max_summary_width = inner.width.saturating_sub(4) as usize;
    let summary = if request.tool_input_summary.len() > max_summary_width {
        format!("{}…", &request.tool_input_summary[..max_summary_width.saturating_sub(1)])
    } else {
        request.tool_input_summary.clone()
    };
    let summary_line = Line::from(vec![
        Span::styled("  ", Style::default()),
        Span::styled(summary, Style::default().fg(Color::White)),
    ]);
    frame.render_widget(Paragraph::new(summary_line).wrap(Wrap { trim: false }), chunks[2]);

    // Key hints
    let hints = Line::from(vec![
        Span::styled("  ", Style::default()),
        Span::styled("y", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
        Span::styled(" allow  ", Style::default().fg(Color::DarkGray)),
        Span::styled("n", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
        Span::styled(" deny  ", Style::default().fg(Color::DarkGray)),
        Span::styled("a", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        Span::styled(" always allow", Style::default().fg(Color::DarkGray)),
    ]);
    frame.render_widget(Paragraph::new(hints), chunks[4]);
}
