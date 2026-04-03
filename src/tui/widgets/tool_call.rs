//! Tool call rendering — styled one-liners with spinner/check/cross prefix.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::tui::app::ToolCallStatus;

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Render a tool call as a single styled Line.
pub fn render_tool_call(
    name: &str,
    summary: &str,
    status: &ToolCallStatus,
    tick: usize,
) -> Line<'static> {
    let (prefix, prefix_style) = match status {
        ToolCallStatus::Running => (
            SPINNER_FRAMES[tick % SPINNER_FRAMES.len()].to_string(),
            Style::default().fg(Color::Cyan),
        ),
        ToolCallStatus::Success => (
            "✔".to_string(),
            Style::default().fg(Color::Green),
        ),
        ToolCallStatus::Error => (
            "✖".to_string(),
            Style::default().fg(Color::Red),
        ),
    };

    let name_color = tool_name_color(name);
    let name_style = Style::default()
        .fg(name_color)
        .add_modifier(Modifier::BOLD);

    let mut spans = vec![
        Span::raw("  "),
        Span::styled(prefix, prefix_style),
        Span::raw(" "),
        Span::styled(name.to_string(), name_style),
    ];

    if !summary.is_empty() {
        spans.push(Span::styled(
            format!("  {}", summary),
            Style::default().fg(Color::DarkGray),
        ));
    }

    Line::from(spans)
}

/// Render a collapsed group of successful tool calls as a single line.
pub fn render_tool_call_group(calls: &[(&str, &str)]) -> Line<'static> {
    let count = calls.len();
    let summary = if count <= 3 {
        calls
            .iter()
            .map(|(n, _)| *n)
            .collect::<Vec<_>>()
            .join(", ")
    } else {
        format!(
            "{}, {} and {} more",
            calls[0].0,
            calls[1].0,
            count - 2
        )
    };
    Line::from(vec![
        Span::raw("  "),
        Span::styled(
            "\u{2714}",
            Style::default().fg(Color::Green),
        ),
        Span::raw(" "),
        Span::styled(summary, Style::default().fg(Color::DarkGray)),
    ])
}

fn tool_name_color(name: &str) -> Color {
    match name {
        "Read" | "Write" | "Edit" | "NotebookEdit" => Color::Yellow,
        "Bash" | "Shell" => Color::Red,
        "Grep" | "Glob" | "WebSearch" | "WebFetch" => Color::Cyan,
        "Think" => Color::Magenta,
        _ => Color::Blue,
    }
}
