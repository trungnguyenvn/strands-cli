//! Input bar widget — wraps tui-textarea with submit/newline key handling.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::Block;
use ratatui::Frame;

use ratatui::text::Span;
use ratatui::widgets::Paragraph;

use crate::commands;
use crate::tui::app::{AgentStatus, AppState};

/// Returns the desired height for the input area (min 3, max 40% of terminal).
pub fn input_height(state: &AppState, terminal_height: u16) -> u16 {
    let line_count = state.input.lines().len() as u16;
    let max_h = (terminal_height * 2 / 5).max(3);
    (line_count + 2).clamp(3, max_h) // +2 for border
}

/// Render the input bar.
pub fn render_input(state: &mut AppState, frame: &mut Frame, area: Rect) {
    let title = match &state.agent_status {
        AgentStatus::Idle => " Enter to send │ Alt+Enter newline │ /exit quit ",
        AgentStatus::Streaming => " Streaming... (Esc to cancel) ",
        AgentStatus::Error(_) => " Error occurred │ Enter to retry ",
    };

    let border_color = match &state.agent_status {
        AgentStatus::Idle => Color::Cyan,
        AgentStatus::Streaming => Color::Yellow,
        AgentStatus::Error(_) => Color::Red,
    };

    let block = Block::bordered()
        .title(title)
        .border_style(Style::default().fg(border_color));

    state.input.set_block(block);
    frame.render_widget(&state.input, area);

    // Render inline argument hint (mirrors Claude Code's BaseTextInput argumentHint).
    // Shows dimmed hint text after the command name when the user has typed
    // "/command " (command + single trailing space, no real args yet).
    render_argument_hint(state, frame, area);
}

/// Render a dimmed argument hint inline after the cursor when typing a slash command.
/// Mirrors Claude Code's `showArgumentHint` in `BaseTextInput.tsx`.
fn render_argument_hint(state: &AppState, frame: &mut Frame, area: Rect) {
    let text = state.input.lines().join("\n");
    let trimmed = text.trim_start();

    if !trimmed.starts_with('/') {
        return;
    }

    // Only show hint when: "/command " with trailing space but no real args yet.
    // This matches Claude Code: `commandWithoutArgs && props.value.startsWith("/")`
    let has_trailing_space = text.ends_with(' ');
    let has_real_args = {
        let space_idx = trimmed.find(' ');
        space_idx
            .map(|i| trimmed[i + 1..].trim().len() > 0)
            .unwrap_or(false)
    };

    if !has_trailing_space || has_real_args {
        return;
    }

    let parsed = match commands::parse_slash_command(trimmed) {
        Some(p) => p,
        None => return,
    };

    let hint = match state.command_registry.find(&parsed.command_name) {
        Some(cmd) => match &cmd.argument_hint {
            Some(h) => h.as_str(),
            None => return,
        },
        None => return,
    };

    // Position the hint after the input text, inside the bordered area.
    // The border takes 1 col on each side, so inner x = area.x + 1.
    // The cursor is at column (text.len()), clamped to the inner width.
    let inner_x = area.x + 1;
    let inner_width = area.width.saturating_sub(2);
    let text_len = text.len() as u16;

    if text_len >= inner_width {
        return; // no room for hint
    }

    let hint_x = inner_x + text_len;
    let hint_width = inner_width.saturating_sub(text_len);
    let hint_area = Rect::new(hint_x, area.y + 1, hint_width, 1);

    let hint_widget = Paragraph::new(Span::styled(
        hint,
        Style::default().fg(Color::DarkGray),
    ));
    frame.render_widget(hint_widget, hint_area);
}

/// Process a key event for the input bar. Returns true if the key was consumed.
pub fn handle_input_key(state: &mut AppState, key: KeyEvent) -> InputAction {
    match (key.modifiers, key.code) {
        // Submit
        (KeyModifiers::NONE, KeyCode::Enter) => InputAction::Submit,

        // Newline (Alt+Enter or Shift+Enter)
        (KeyModifiers::ALT, KeyCode::Enter) | (KeyModifiers::SHIFT, KeyCode::Enter) => {
            state.input.insert_newline();
            InputAction::Consumed
        }

        // History: Up arrow on first line
        (KeyModifiers::NONE, KeyCode::Up) => {
            let cursor_row = state.input.cursor().0;
            if cursor_row == 0 && !state.input_history.is_empty() {
                // Stash current edit when starting browse
                if state.history_index.is_none() {
                    state.history_stash = state.input.lines().join("\n");
                }
                let new_index = match state.history_index {
                    None => Some(state.input_history.len() - 1),
                    Some(0) => Some(0),
                    Some(i) => Some(i - 1),
                };
                if let Some(idx) = new_index {
                    state.history_index = new_index;
                    let text = state.input_history[idx].clone();
                    replace_textarea_content(state, &text);
                }
                InputAction::Consumed
            } else {
                state.input.input(crossterm::event::Event::Key(key));
                InputAction::Consumed
            }
        }

        // History: Down arrow on last line
        (KeyModifiers::NONE, KeyCode::Down) => {
            let total_lines = state.input.lines().len();
            let cursor_row = state.input.cursor().0;
            if cursor_row == total_lines.saturating_sub(1) && state.history_index.is_some() {
                let current = state.history_index.unwrap();
                if current + 1 >= state.input_history.len() {
                    // Past the end: restore stash
                    state.history_index = None;
                    let stash = state.history_stash.clone();
                    replace_textarea_content(state, &stash);
                } else {
                    state.history_index = Some(current + 1);
                    let text = state.input_history[current + 1].clone();
                    replace_textarea_content(state, &text);
                }
                InputAction::Consumed
            } else {
                state.input.input(crossterm::event::Event::Key(key));
                InputAction::Consumed
            }
        }

        // Forward everything else to textarea
        _ => {
            state
                .input
                .input(crossterm::event::Event::Key(key));
            InputAction::Consumed
        }
    }
}

fn replace_textarea_content(state: &mut AppState, text: &str) {
    state.input = tui_textarea::TextArea::default();
    state.input.set_cursor_line_style(ratatui::style::Style::default());
    state.input.set_placeholder_text("Type a message... (Enter to send, Alt+Enter for newline)");
    for (i, line) in text.lines().enumerate() {
        if i > 0 {
            state.input.insert_newline();
        }
        for ch in line.chars() {
            state.input.insert_char(ch);
        }
    }
}

pub enum InputAction {
    Submit,
    Consumed,
}
