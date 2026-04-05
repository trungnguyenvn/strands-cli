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

/// Render a collapsed read/search group as a count badge.
///
/// Matches TS's `CollapsedReadSearchContent` component from `collapseReadSearchGroups()`.
///
/// Rules:
/// - Only include non-zero category counts.
/// - If only reads with ≤3 file paths, show paths instead of a count (e.g. "README.md, src/lib.rs").
/// - If exactly 1 read and no other categories, show the single path directly.
/// - Prefix with green ✔ checkmark; counts/paths styled in DarkGray.
pub fn render_collapsed_read_search(
    reads: usize,
    searches: usize,
    fetches: usize,
    file_paths: &[&str],
) -> Line<'static> {
    let mut parts: Vec<String> = Vec::new();

    // If only reads, show file paths instead of a count when there are ≤3 of them.
    if searches == 0 && fetches == 0 && reads > 0 && file_paths.len() <= 3 {
        let paths_display = file_paths.join(", ");
        return Line::from(vec![
            Span::raw("  "),
            Span::styled("✔", Style::default().fg(Color::Green)),
            Span::raw(" "),
            Span::styled(paths_display, Style::default().fg(Color::DarkGray)),
        ]);
    }

    if reads > 0 {
        parts.push(format!("{} {}", reads, if reads == 1 { "read" } else { "reads" }));
    }
    if searches > 0 {
        parts.push(format!(
            "{} {}",
            searches,
            if searches == 1 { "search" } else { "searches" }
        ));
    }
    if fetches > 0 {
        parts.push(format!("{} {}", fetches, if fetches == 1 { "fetch" } else { "fetches" }));
    }

    let summary = parts.join(" · ");
    Line::from(vec![
        Span::raw("  "),
        Span::styled("✔", Style::default().fg(Color::Green)),
        Span::raw(" "),
        Span::styled(summary, Style::default().fg(Color::DarkGray)),
    ])
}

/// Render a tool result as styled lines.
///
/// Errors: red `✖` prefix + red dimmed text (first 3 lines).
/// Success with content: dim gray indented text (first 3 lines).
/// Indented with 4 spaces to align under tool call content.
pub fn render_tool_result(text: &str, is_error: bool) -> Vec<Line<'static>> {
    // Take up to 4 lines to detect truncation without collecting everything.
    let mut collected: Vec<&str> = text.lines().take(4).collect();
    let truncated = collected.len() == 4;
    if truncated {
        collected.pop();
    }

    let mut result = Vec::new();
    let last_idx = collected.len().saturating_sub(1);

    for (i, line_text) in collected.iter().enumerate() {
        let suffix = if i == last_idx && truncated { "…" } else { "" };
        let indented = format!("    {}{}", line_text, suffix);

        let line = if is_error {
            let text_style = Style::default().fg(Color::Red).add_modifier(Modifier::DIM);
            if i == 0 {
                Line::from(vec![
                    Span::raw("  "),
                    Span::styled("✖ ", Style::default().fg(Color::Red)),
                    Span::styled(format!("{}{}", line_text, suffix), text_style),
                ])
            } else {
                Line::from(vec![Span::styled(indented, text_style)])
            }
        } else {
            Line::from(vec![Span::styled(
                indented,
                Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM),
            )])
        };
        result.push(line);
    }

    result
}

/// Render a thinking/reasoning block as collapsed dim lines with a prefix.
///
/// Shows a "thinking…" indicator — matches TS's collapsed thinking display.
pub fn render_thinking_block(text: &str) -> Vec<Line<'static>> {
    if text.is_empty() {
        return Vec::new();
    }
    let line_count = text.lines().count();
    let label = format!(
        "  ◈ thinking ({} line{})",
        line_count,
        if line_count == 1 { "" } else { "s" }
    );
    vec![Line::from(Span::styled(
        label,
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM),
    ))]
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
