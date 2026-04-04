//! Markdown → ratatui Line/Span converter using pulldown-cmark + syntect.

use std::sync::LazyLock;

use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;
use unicode_width::UnicodeWidthStr;

static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
static THEME_SET: LazyLock<ThemeSet> = LazyLock::new(ThemeSet::load_defaults);

/// Parse a markdown string into a vector of styled ratatui Lines.
pub fn markdown_to_lines(text: &str, max_width: u16) -> Vec<Line<'static>> {
    let parser = Parser::new(text);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current_spans: Vec<Span<'static>> = Vec::new();
    let mut style_stack: Vec<Style> = vec![Style::default()];
    let mut in_code_block = false;
    let mut code_block_buf = String::new();
    let mut code_block_lang = String::new();

    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Heading { level, .. } => {
                    flush_line(&mut current_spans, &mut lines);
                    lines.push(Line::from(""));
                    let style = match level {
                        pulldown_cmark::HeadingLevel::H1 => {
                            Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
                        }
                        pulldown_cmark::HeadingLevel::H2 => {
                            Style::default().add_modifier(Modifier::BOLD)
                        }
                        _ => Style::default()
                            .add_modifier(Modifier::BOLD | Modifier::ITALIC),
                    };
                    style_stack.push(style);
                }
                Tag::Strong => {
                    let base = current_style(&style_stack);
                    style_stack.push(base.add_modifier(Modifier::BOLD));
                }
                Tag::Emphasis => {
                    let base = current_style(&style_stack);
                    style_stack.push(base.add_modifier(Modifier::ITALIC));
                }
                Tag::CodeBlock(kind) => {
                    flush_line(&mut current_spans, &mut lines);
                    in_code_block = true;
                    code_block_buf.clear();
                    code_block_lang = match kind {
                        pulldown_cmark::CodeBlockKind::Fenced(lang) => {
                            lang.split_whitespace().next().unwrap_or("").to_string()
                        }
                        pulldown_cmark::CodeBlockKind::Indented => String::new(),
                    };
                }
                Tag::BlockQuote(_) => {
                    flush_line(&mut current_spans, &mut lines);
                    let base = current_style(&style_stack);
                    style_stack.push(base.fg(Color::Cyan));
                }
                Tag::List(_) => {}
                Tag::Item => {
                    flush_line(&mut current_spans, &mut lines);
                    current_spans.push(Span::styled(
                        "  \u{2022} ",
                        Style::default().fg(Color::Cyan),
                    ));
                }
                Tag::Paragraph => {
                    flush_line(&mut current_spans, &mut lines);
                }
                Tag::Link { .. } => {}
                _ => {}
            },
            Event::End(tag_end) => match tag_end {
                TagEnd::Heading(_) => {
                    style_stack.pop();
                    flush_line(&mut current_spans, &mut lines);
                }
                TagEnd::Strong | TagEnd::Emphasis => {
                    style_stack.pop();
                }
                TagEnd::CodeBlock => {
                    in_code_block = false;
                    lines.extend(render_code_block(
                        &code_block_buf,
                        &code_block_lang,
                        max_width,
                    ));
                    code_block_buf.clear();
                    code_block_lang.clear();
                }
                TagEnd::BlockQuote(_) => {
                    style_stack.pop();
                    flush_line(&mut current_spans, &mut lines);
                }
                TagEnd::Item => {
                    flush_line(&mut current_spans, &mut lines);
                }
                TagEnd::Paragraph => {
                    flush_line(&mut current_spans, &mut lines);
                }
                _ => {}
            },
            Event::Text(text) => {
                if in_code_block {
                    code_block_buf.push_str(&text);
                } else {
                    let style = current_style(&style_stack);
                    current_spans.push(Span::styled(text.to_string(), style));
                }
            }
            Event::Code(code) => {
                current_spans.push(Span::styled(
                    format!(" {} ", code),
                    Style::default()
                        .fg(Color::Yellow)
                        .bg(Color::Rgb(40, 40, 40)),
                ));
            }
            Event::SoftBreak | Event::HardBreak => {
                flush_line(&mut current_spans, &mut lines);
            }
            Event::Rule => {
                flush_line(&mut current_spans, &mut lines);
                lines.push(Line::from(Span::styled(
                    "\u{2500}".repeat(40),
                    Style::default().fg(Color::DarkGray),
                )));
            }
            _ => {}
        }
    }

    flush_line(&mut current_spans, &mut lines);
    lines
}

fn current_style(stack: &[Style]) -> Style {
    stack.last().copied().unwrap_or_default()
}

fn flush_line(spans: &mut Vec<Span<'static>>, lines: &mut Vec<Line<'static>>) {
    if !spans.is_empty() {
        lines.push(Line::from(std::mem::take(spans)));
    }
}

fn render_code_block(code: &str, lang: &str, max_width: u16) -> Vec<Line<'static>> {
    let mut result: Vec<Line<'static>> = Vec::new();

    // Adaptive box width based on content
    let max_code_width = code
        .lines()
        .map(|l| UnicodeWidthStr::width(l))
        .max()
        .unwrap_or(0);
    let available = (max_width as usize).saturating_sub(8); // margin for "  │ " prefix + padding
    let box_inner = max_code_width.min(available).max(20);

    // Top border with language label
    let top_border = if lang.is_empty() {
        format!("  \u{250c}{}\u{2510}", "\u{2500}".repeat(box_inner + 2))
    } else {
        let label_len = lang.len() + 2; // " lang "
        let remaining = (box_inner + 2).saturating_sub(label_len);
        format!(
            "  \u{250c} {} {}\u{2510}",
            lang,
            "\u{2500}".repeat(remaining)
        )
    };
    result.push(Line::from(Span::styled(
        top_border,
        Style::default().fg(Color::DarkGray),
    )));

    // Try syntax highlighting, fall back to plain green
    match try_highlight(code, lang) {
        Some(highlighted_lines) => result.extend(highlighted_lines),
        None => {
            for line in code.lines() {
                result.push(Line::from(vec![
                    Span::styled("  \u{2502} ", Style::default().fg(Color::DarkGray)),
                    Span::styled(line.to_string(), Style::default().fg(Color::Green)),
                ]));
            }
        }
    }

    // Bottom border
    let bottom_border = format!(
        "  \u{2514}{}\u{2518}",
        "\u{2500}".repeat(box_inner + 2)
    );
    result.push(Line::from(Span::styled(
        bottom_border,
        Style::default().fg(Color::DarkGray),
    )));

    result
}

fn try_highlight(code: &str, lang: &str) -> Option<Vec<Line<'static>>> {
    if lang.is_empty() {
        return None;
    }

    let syntax = SYNTAX_SET
        .find_syntax_by_token(lang)
        .or_else(|| SYNTAX_SET.find_syntax_by_extension(lang))?;

    let theme = THEME_SET
        .themes
        .get("base16-ocean.dark")
        .or_else(|| THEME_SET.themes.values().next())?;

    let mut h = HighlightLines::new(syntax, theme);
    let mut lines = Vec::new();

    for line in code.lines() {
        // syntect expects lines with newline
        let line_nl = format!("{}\n", line);
        let ranges = h.highlight_line(&line_nl, &SYNTAX_SET).ok()?;
        let mut spans = vec![Span::styled(
            "  \u{2502} ",
            Style::default().fg(Color::DarkGray),
        )];
        for (syntect_style, text) in ranges {
            let trimmed = text.trim_end_matches('\n');
            if !trimmed.is_empty() {
                let fg = syntect_color_to_ratatui(syntect_style.foreground);
                spans.push(Span::styled(trimmed.to_string(), Style::default().fg(fg)));
            }
        }
        lines.push(Line::from(spans));
    }

    Some(lines)
}

fn syntect_color_to_ratatui(c: syntect::highlighting::Color) -> Color {
    Color::Rgb(c.r, c.g, c.b)
}

/// Find the byte offset of the last stable block boundary in markdown text.
/// A boundary is a `\n\n` (blank line) that is NOT inside a code fence.
/// Everything before this offset consists of complete markdown blocks that
/// will not be affected by text appended after the offset.
///
/// Used by the incremental streaming renderer to avoid re-parsing the entire
/// text on every frame — only the unstable suffix after the boundary is re-parsed.
pub fn find_stable_boundary(text: &str) -> usize {
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut fence_count = 0usize;
    let mut boundary = 0usize;
    let mut i = 0;

    while i < len {
        // Check for code fence at start of line
        if i == 0 || bytes[i - 1] == b'\n' {
            let trimmed_start = skip_leading_spaces(bytes, i);
            if trimmed_start + 2 < len
                && bytes[trimmed_start] == b'`'
                && bytes[trimmed_start + 1] == b'`'
                && bytes[trimmed_start + 2] == b'`'
            {
                fence_count += 1;
            }
        }

        // Check for \n\n (block boundary) outside code fences
        if bytes[i] == b'\n'
            && i + 1 < len
            && bytes[i + 1] == b'\n'
            && fence_count % 2 == 0
        {
            boundary = i + 2; // after the \n\n
        }

        i += 1;
    }

    boundary
}

fn skip_leading_spaces(bytes: &[u8], start: usize) -> usize {
    let mut i = start;
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
        i += 1;
    }
    i
}
