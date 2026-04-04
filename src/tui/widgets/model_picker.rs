//! Interactive model picker overlay — rendered over the message area.
//!
//! Mirrors Claude Code's `<ModelPicker>` + `<Select>` component:
//! - Up/Down to navigate, Enter to confirm, Esc to cancel
//! - Shows models grouped by provider with section headers
//! - Highlights the currently focused item with `❯` prefix
//! - Marks the current model with `✓`

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use super::super::app::ModelPickerState;

const MAX_VISIBLE: usize = 16;

/// Render the model picker as a centered overlay.
pub fn render_model_picker(picker: &ModelPickerState, frame: &mut Frame, area: Rect) {
    if picker.items.is_empty() {
        return;
    }

    // Build lines: header + grouped items
    let mut all_lines: Vec<PickerLine> = Vec::new();

    // Header
    all_lines.push(PickerLine::Header("Select model (↑↓ navigate, Enter confirm, Esc cancel)".to_string()));
    all_lines.push(PickerLine::Blank);

    // Group items by their group field, preserving order
    let mut current_group: Option<&str> = None;
    for (i, item) in picker.items.iter().enumerate() {
        if current_group != Some(&item.group) {
            if current_group.is_some() {
                all_lines.push(PickerLine::Blank);
            }
            all_lines.push(PickerLine::GroupHeader(item.group.clone()));
            current_group = Some(&item.group);
        }
        let is_current = item.model_id == picker.current_model
            || item.alias == picker.current_model;
        all_lines.push(PickerLine::Item {
            index: i,
            alias: item.alias.clone(),
            label: item.label.clone(),
            is_focused: i == picker.focused,
            is_current,
        });
    }

    let total_lines = all_lines.len();

    // Calculate visible window centered on focused item
    let focused_line = all_lines
        .iter()
        .position(|l| matches!(l, PickerLine::Item { is_focused: true, .. }))
        .unwrap_or(0);

    let visible = total_lines.min(MAX_VISIBLE);
    let start = if total_lines <= visible {
        0
    } else {
        focused_line
            .saturating_sub(visible / 2)
            .min(total_lines - visible)
    };
    let end = (start + visible).min(total_lines);

    // Find max alias width for alignment
    let max_alias = picker.items.iter().map(|i| i.alias.len()).max().unwrap_or(0);

    // Render lines
    let mut rendered: Vec<Line> = Vec::new();
    for line in &all_lines[start..end] {
        rendered.push(render_line(line, max_alias));
    }

    // Calculate overlay dimensions
    let picker_width = (area.width.saturating_sub(4)).min(70);
    let picker_height = (rendered.len() as u16 + 2).min(area.height.saturating_sub(2)); // +2 for border
    let x = area.x + (area.width.saturating_sub(picker_width)) / 2;
    let y = area.y + (area.height.saturating_sub(picker_height)) / 2;
    let picker_area = Rect::new(x, y, picker_width, picker_height);

    // Clear the area behind the picker
    frame.render_widget(Clear, picker_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(" /model ")
        .title_style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD));

    let paragraph = Paragraph::new(rendered)
        .block(block)
        .style(Style::default().bg(Color::Rgb(20, 20, 30)));

    frame.render_widget(paragraph, picker_area);
}

#[derive(Debug)]
enum PickerLine {
    Header(String),
    GroupHeader(String),
    Blank,
    Item {
        #[allow(dead_code)]
        index: usize,
        alias: String,
        label: String,
        is_focused: bool,
        is_current: bool,
    },
}

fn render_line(line: &PickerLine, max_alias: usize) -> Line<'static> {
    match line {
        PickerLine::Header(text) => Line::from(Span::styled(
            format!(" {}", text),
            Style::default().fg(Color::Gray),
        )),
        PickerLine::GroupHeader(group) => Line::from(Span::styled(
            format!(" {}:", group),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        PickerLine::Blank => Line::from(""),
        PickerLine::Item {
            alias,
            label,
            is_focused,
            is_current,
            ..
        } => {
            let prefix = if *is_focused { " ❯ " } else { "   " };
            let check = if *is_current { " ✓" } else { "" };
            let padded = format!("{:<width$}", alias, width = max_alias);

            if *is_focused {
                Line::from(vec![
                    Span::styled(
                        prefix,
                        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        padded,
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("  {}", label),
                        Style::default().fg(Color::Gray),
                    ),
                    Span::styled(
                        check.to_string(),
                        Style::default().fg(Color::Green),
                    ),
                ])
            } else {
                let base_color = if *is_current {
                    Color::Green
                } else {
                    Color::DarkGray
                };
                Line::from(vec![
                    Span::styled(prefix, Style::default().fg(base_color)),
                    Span::styled(padded, Style::default().fg(base_color)),
                    Span::styled(
                        format!("  {}", label),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::styled(
                        check.to_string(),
                        Style::default().fg(Color::Green),
                    ),
                ])
            }
        }
    }
}
