//! Message history widget — renders all chat messages as scrollable content.
//!
//! Performance architecture (mirrors Claude Code's 4-layer approach):
//!
//! 1. **Per-message cache**: Each message's rendered lines are cached and validated
//!    by fingerprint (block_count, text_len, width). Old messages never re-render.
//! 2. **Incremental streaming markdown**: The streaming message's last text block
//!    tracks a "stable prefix boundary". Only the unstable suffix after the last
//!    complete markdown block is re-parsed each frame.
//! 3. **Viewport slicing**: Only messages overlapping the visible viewport (+overscan)
//!    have their lines cloned into the final Paragraph. Off-screen messages contribute
//!    only their line count for scroll math.
//! 4. **Eliminated redundant clones**: Per-message caches store owned lines. Assembly
//!    only clones lines for visible messages, not the entire history.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap};
use ratatui::Frame;

use crate::tui::app::{
    AgentStatus, AppState, ChatMessage, ContentBlock, MessageCacheEntry, Role,
    StreamingMdCache, ToolCallStatus,
};
use crate::tui::widgets::markdown::{find_stable_boundary, markdown_to_lines};
use crate::tui::widgets::tool_call::{render_tool_call, render_tool_call_group};

/// Left margin prefix for all message content.
const MARGIN: &str = "  ";

/// Extra lines rendered above/below the visible viewport.
const OVERSCAN: u16 = 40;

pub fn render_messages(state: &mut AppState, frame: &mut Frame, area: Rect) {
    // Store the messages area for selection coordinate mapping
    state.selection.messages_area = area;

    // Welcome header when no messages
    if state.messages.is_empty() {
        let welcome = render_welcome(&state.model_name);
        state.selection.rendered_lines = welcome.iter().map(line_to_plain_text).collect();
        state.selection.rendered_y_scroll = 0;
        let para = Paragraph::new(welcome);
        frame.render_widget(para, area);
        return;
    }

    let is_streaming = matches!(state.agent_status, AgentStatus::Streaming);
    let msg_count = state.messages.len();
    let width = state.terminal_width;

    // ---------------------------------------------------------------
    // Pass 1: Ensure per-message caches are up-to-date.
    //         Collect per-message line counts for scroll math.
    // ---------------------------------------------------------------

    // Sync cache vector length with messages
    while state.message_cache.len() < msg_count {
        state.message_cache.push(None);
    }
    state.message_cache.truncate(msg_count);

    // Per-message line counts (used for viewport slicing + total line calculation)
    let mut line_counts: Vec<u16> = Vec::with_capacity(msg_count);

    // The area width used for wrap calculations. Paragraph with Wrap wraps
    // at this width, so we must count wrapped (visual) lines, not logical ones.
    // On narrow terminals (mobile), lines wrap frequently — using logical counts
    // would make y_scroll too small, causing content to appear stuck under the input.
    let wrap_width = area.width;

    for i in 0..msg_count {
        let is_last_streaming = i == msg_count - 1 && is_streaming;

        if is_last_streaming {
            // Streaming message: use incremental renderer (not per-message cache)
            let lines = render_streaming_message(
                &state.messages[i],
                &mut state.streaming_md_cache,
                state.tick_count,
                width,
            );
            let count = paragraph_line_count(&lines, wrap_width);
            line_counts.push(count);
            // Store in message_cache slot temporarily for Pass 2
            state.message_cache[i] = Some(MessageCacheEntry {
                lines,
                block_count: 0,
                text_len: 0,
                width: 0, // fingerprint intentionally invalid so it's never reused as-is
                wrapped_line_count: count,
                wrap_width,
            });
        } else {
            // Check per-message cache validity
            let valid = state.message_cache[i]
                .as_ref()
                .map_or(false, |c| c.is_valid(&state.messages[i], width));

            if !valid {
                let lines = render_message(&state.messages[i], state.tick_count, width);
                state.message_cache[i] = Some(MessageCacheEntry::new(
                    lines,
                    &state.messages[i],
                    width,
                ));
            }

            let entry = state.message_cache[i].as_mut().unwrap();
            // Use cached wrapped line count if wrap_width hasn't changed
            if entry.wrap_width != wrap_width {
                entry.wrapped_line_count = paragraph_line_count(&entry.lines, wrap_width);
                entry.wrap_width = wrap_width;
            }
            line_counts.push(entry.wrapped_line_count);
        }
    }

    // ---------------------------------------------------------------
    // Scroll math (using wrapped/visual line counts)
    // ---------------------------------------------------------------

    // Total visual lines = sum of wrapped per-message lines + separators + bottom padding
    let separator_lines = if msg_count > 1 { (msg_count - 1) as u16 } else { 0 };
    let total_lines: u16 = line_counts.iter().copied().sum::<u16>()
        .saturating_add(separator_lines)
        .saturating_add(1); // bottom padding

    state.total_lines = total_lines;
    let visible = area.height;
    let max_offset = total_lines.saturating_sub(visible);
    let y_scroll = if state.auto_scroll {
        max_offset
    } else {
        max_offset.saturating_sub(state.scroll_offset)
    };

    // ---------------------------------------------------------------
    // Pass 2: Viewport slicing — only clone lines for visible messages.
    //         Off-screen messages contribute placeholder empty lines
    //         to preserve scroll position math.
    // ---------------------------------------------------------------

    let view_start = y_scroll.saturating_sub(OVERSCAN);
    let view_end = (y_scroll + visible).saturating_add(OVERSCAN).min(total_lines);

    let mut all_lines: Vec<Line<'static>> = Vec::with_capacity(total_lines as usize);
    let mut cumulative: u16 = 0;

    for i in 0..msg_count {
        // Separator between messages
        if i > 0 {
            let sep_pos = cumulative;
            cumulative += 1;
            if sep_pos >= view_start && sep_pos < view_end {
                all_lines.push(Line::from(""));
            } else {
                all_lines.push(Line::from("")); // placeholder (cheap)
            }
        }

        let msg_line_count = line_counts[i];
        let msg_start = cumulative;
        let msg_end = cumulative + msg_line_count;

        // Check if this message overlaps the visible viewport (with overscan)
        let is_visible = msg_end > view_start && msg_start < view_end;

        if is_visible {
            // Clone lines from cache into the output
            if let Some(ref entry) = state.message_cache[i] {
                all_lines.extend(entry.lines.iter().cloned());
            }
        } else {
            // Off-screen: push cheap placeholder lines to maintain correct total count
            for _ in 0..msg_line_count {
                all_lines.push(Line::from(""));
            }
        }

        cumulative = msg_end;
    }

    // Bottom padding
    all_lines.push(Line::from(""));

    // ---------------------------------------------------------------
    // Render
    // ---------------------------------------------------------------

    // Selection text extraction (only when actively selecting)
    if state.selection.active || state.selection.anchor != state.selection.end {
        state.selection.rendered_lines = all_lines.iter().map(line_to_plain_text).collect();
    }
    state.selection.rendered_y_scroll = y_scroll;

    let paragraph = Paragraph::new(all_lines)
        .wrap(Wrap { trim: false })
        .scroll((y_scroll, 0));

    frame.render_widget(paragraph, area);

    // Selection highlight overlay
    render_selection_highlight(state, frame, area, y_scroll);

    // Scrollbar
    if total_lines > visible {
        let mut scrollbar_state =
            ScrollbarState::new(max_offset as usize).position(y_scroll as usize);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight),
            area,
            &mut scrollbar_state,
        );
    }
}

// ---------------------------------------------------------------------------
// Message rendering
// ---------------------------------------------------------------------------

/// Render a single settled (non-streaming) message into styled lines.
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
            render_assistant_blocks(&msg.blocks, tick, max_width, &margin, &mut lines);
        }
    }

    lines
}

/// Render assistant message content blocks (text + tool calls with grouping).
fn render_assistant_blocks(
    blocks: &[ContentBlock],
    tick: usize,
    max_width: u16,
    margin: &Span<'static>,
    lines: &mut Vec<Line<'static>>,
) {
    let mut i = 0;
    while i < blocks.len() {
        match &blocks[i] {
            ContentBlock::Text(text) => {
                if !text.is_empty() {
                    let md_width = max_width.saturating_sub(MARGIN.len() as u16);
                    for mut line in markdown_to_lines(text, md_width) {
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

// ---------------------------------------------------------------------------
// Streaming message: incremental markdown rendering (Layer 2)
// ---------------------------------------------------------------------------

/// Render the actively streaming message using incremental markdown caching.
///
/// For the last (growing) text block:
/// 1. Find the stable prefix boundary (last `\n\n` outside code fences)
/// 2. Reuse cached lines for text before the boundary
/// 3. Only re-parse markdown for text after the boundary
/// 4. Apply line-buffering: truncate the unstable suffix at the last newline
///
/// All other blocks (earlier text blocks, tool calls) are rendered normally.
fn render_streaming_message(
    msg: &ChatMessage,
    streaming_cache: &mut Option<StreamingMdCache>,
    tick: usize,
    max_width: u16,
) -> Vec<Line<'static>> {
    let margin = Span::raw(MARGIN);
    let mut lines = Vec::new();
    let md_width = max_width.saturating_sub(MARGIN.len() as u16);

    // Find the index of the last Text block (the one actively growing)
    let last_text_idx = msg
        .blocks
        .iter()
        .rposition(|b| matches!(b, ContentBlock::Text(_)));

    for (block_idx, block) in msg.blocks.iter().enumerate() {
        match block {
            ContentBlock::Text(text) if Some(block_idx) == last_text_idx => {
                // This is the growing text block — use incremental rendering
                let text_lines =
                    render_streaming_text_incremental(text, streaming_cache, md_width);
                for mut line in text_lines {
                    line.spans.insert(0, margin.clone());
                    lines.push(line);
                }
            }
            ContentBlock::Text(text) => {
                // Earlier stable text block — render normally (typically small)
                if !text.is_empty() {
                    for mut line in markdown_to_lines(text, md_width) {
                        line.spans.insert(0, margin.clone());
                        lines.push(line);
                    }
                }
            }
            ContentBlock::ToolCall {
                name,
                summary,
                status,
                ..
            } => {
                // Tool calls are cheap to render (no markdown), always re-render for spinner
                let mut line = render_tool_call(name, summary, status, tick);
                line.spans.insert(0, margin.clone());
                lines.push(line);
            }
        }
    }

    lines
}

/// Incrementally render a growing text block using the streaming markdown cache.
///
/// Stable prefix (complete paragraphs/code blocks) → cached, never re-parsed.
/// Unstable suffix (last incomplete block) → re-parsed each frame, line-buffered.
fn render_streaming_text_incremental(
    text: &str,
    cache: &mut Option<StreamingMdCache>,
    max_width: u16,
) -> Vec<Line<'static>> {
    if text.is_empty() {
        return Vec::new();
    }

    let new_boundary = find_stable_boundary(text);

    // Update the streaming cache
    match cache {
        Some(ref mut c) if new_boundary > c.boundary => {
            // Boundary advanced — render the delta (newly completed blocks) and append
            let delta = &text[c.boundary..new_boundary];
            if !delta.trim().is_empty() {
                let delta_lines = markdown_to_lines(delta, max_width);
                c.prefix_lines.extend(delta_lines);
            }
            c.boundary = new_boundary;
        }
        Some(ref c) if new_boundary == c.boundary => {
            // Boundary unchanged — reuse cached prefix as-is
        }
        _ => {
            // No cache, or boundary moved backward (shouldn't happen) — build from scratch
            let prefix_lines = if new_boundary > 0 {
                let prefix = &text[..new_boundary];
                markdown_to_lines(prefix, max_width)
            } else {
                Vec::new()
            };
            *cache = Some(StreamingMdCache {
                boundary: new_boundary,
                prefix_lines,
            });
        }
    }

    // Assemble: cached prefix + freshly-parsed unstable suffix
    let prefix_lines = &cache.as_ref().unwrap().prefix_lines;
    let suffix = &text[new_boundary..];

    let mut lines = Vec::with_capacity(prefix_lines.len() + 20);
    lines.extend(prefix_lines.iter().cloned());

    // Line-buffering: only show text up to the last newline in the suffix
    if !suffix.is_empty() {
        let visible = if let Some(nl) = suffix.rfind('\n') {
            &suffix[..nl + 1]
        } else {
            "" // no complete line yet in the suffix
        };
        if !visible.is_empty() {
            lines.extend(markdown_to_lines(visible, max_width));
        }
    }

    lines
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn render_welcome(model_name: &str) -> Vec<Line<'static>> {
    vec![
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
                model_name.to_string(),
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
    ]
}

/// Count the number of visual (wrapped) lines using ratatui's own word-wrapping.
///
/// Previous implementation used character-level ceiling division which underestimates
/// line count vs ratatui's word-boundary wrapping, causing the bottom of message
/// history to be hidden behind the input box.
pub fn paragraph_line_count(lines: &[Line], wrap_width: u16) -> u16 {
    if wrap_width == 0 || lines.is_empty() {
        return lines.len() as u16;
    }
    Paragraph::new(lines.to_vec())
        .wrap(Wrap { trim: false })
        .line_count(wrap_width) as u16
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

        let buf = frame.buffer_mut();
        for col in col_start..col_end {
            if let Some(cell) = buf.cell_mut(ratatui::layout::Position::new(col, row)) {
                let fg = cell.fg;
                let bg = cell.bg;
                cell.set_fg(if bg == Color::Reset { Color::Black } else { bg });
                cell.set_bg(if fg == Color::Reset { Color::White } else { fg });
            }
        }
    }
}
