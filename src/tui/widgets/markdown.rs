//! Markdown → ratatui Line/Span converter using pulldown-cmark.

use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// Parse a markdown string into a vector of styled ratatui Lines.
pub fn markdown_to_lines(text: &str) -> Vec<Line<'static>> {
    let parser = Parser::new(text);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current_spans: Vec<Span<'static>> = Vec::new();
    let mut style_stack: Vec<Style> = vec![Style::default()];
    let mut in_code_block = false;
    let mut code_block_buf = String::new();

    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Heading { level, .. } => {
                    flush_line(&mut current_spans, &mut lines);
                    // Blank line before heading
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
                Tag::CodeBlock(_) => {
                    flush_line(&mut current_spans, &mut lines);
                    in_code_block = true;
                    code_block_buf.clear();
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
                        "  • ",
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
                    // Render code block with border
                    lines.push(Line::from(Span::styled(
                        "  ┌──────────────────────────────────────",
                        Style::default().fg(Color::DarkGray),
                    )));
                    for code_line in code_block_buf.lines() {
                        lines.push(Line::from(vec![
                            Span::styled("  │ ", Style::default().fg(Color::DarkGray)),
                            Span::styled(
                                code_line.to_string(),
                                Style::default().fg(Color::Green),
                            ),
                        ]));
                    }
                    lines.push(Line::from(Span::styled(
                        "  └──────────────────────────────────────",
                        Style::default().fg(Color::DarkGray),
                    )));
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
                // Inline code
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
                    "─".repeat(40),
                    Style::default().fg(Color::DarkGray),
                )));
            }
            _ => {}
        }
    }

    // Flush any remaining spans
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
