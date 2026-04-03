//! Autocomplete suggestion dropdown — rendered above the input bar.
//!
//! Mirrors Claude Code's `PromptInputFooterSuggestions` component.
//! Shows a list of matching commands, with the selected item highlighted.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::commands::SuggestionItem;

const MAX_VISIBLE: usize = 6;

/// Render the autocomplete suggestion dropdown.
/// `area` is the region where suggestions should appear (above the input bar).
pub fn render_suggestions(
    suggestions: &[SuggestionItem],
    selected: i32,
    frame: &mut Frame,
    area: Rect,
) {
    if suggestions.is_empty() || area.height == 0 {
        return;
    }

    let total = suggestions.len();
    let visible_count = total.min(MAX_VISIBLE).min(area.height as usize);

    // Scrolling window centered on selected item
    // (mirrors Claude Code's startIndex calculation)
    let sel = selected.max(0) as usize;
    let start = if total <= visible_count {
        0
    } else {
        sel.saturating_sub(visible_count / 2)
            .min(total - visible_count)
    };
    let end = (start + visible_count).min(total);

    // Find max command name width for alignment
    let max_name_len = suggestions[start..end]
        .iter()
        .map(|s| s.name.len())
        .max()
        .unwrap_or(0);

    let mut lines: Vec<Line> = Vec::with_capacity(visible_count);
    for (i, item) in suggestions[start..end].iter().enumerate() {
        let idx = start + i;
        let is_selected = idx == sel;

        let prefix = if is_selected { "❯ " } else { "  " };
        let padded_name = format!("/{:<width$}", item.name, width = max_name_len);

        // Truncate description to fit
        let desc_width = (area.width as usize)
            .saturating_sub(prefix.len() + padded_name.len() + 4); // 4 = " — " + margin
        let desc = if item.description.len() > desc_width {
            format!("{}…", &item.description[..desc_width.saturating_sub(1)])
        } else {
            item.description.clone()
        };

        let style = if is_selected {
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        let desc_style = if is_selected {
            Style::default().fg(Color::Gray)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        lines.push(Line::from(vec![
            Span::styled(prefix, style),
            Span::styled(padded_name, style),
            Span::styled(" — ", Style::default().fg(Color::DarkGray)),
            Span::styled(desc, desc_style),
        ]));
    }

    // Position the dropdown at the bottom of the area (just above input)
    let dropdown_height = lines.len() as u16;
    let dropdown_y = area.y + area.height.saturating_sub(dropdown_height);
    let dropdown_area = Rect::new(area.x, dropdown_y, area.width, dropdown_height);

    let widget = Paragraph::new(lines).style(Style::default().bg(Color::Rgb(30, 30, 40)));
    frame.render_widget(widget, dropdown_area);
}
