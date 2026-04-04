//! Input bar widget — wraps tui-textarea with submit/newline key handling.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders};
use ratatui::symbols::border;
use ratatui::Frame;

use ratatui::text::Span;
use ratatui::widgets::Paragraph;

use crate::commands;
use crate::tui::app::{AgentStatus, AppState};

/// Returns the desired height for the input area (min 3, max 40% of terminal).
pub fn input_height(state: &AppState, terminal_height: u16) -> u16 {
    let line_count = state.input.lines().len() as u16;
    let max_h = (terminal_height * 2 / 5).max(3);
    (line_count + 2).clamp(3, max_h) // +2 for top and bottom border
}

/// Returns the height of the fly-status line above the input (0 or 1).
pub fn fly_status_height(state: &AppState) -> u16 {
    match &state.agent_status {
        AgentStatus::Idle => 0,
        _ => 1,
    }
}

const SPINNER_VERBS: &[&str] = &[
    "Working", "Thinking", "Analyzing", "Reasoning", "Processing",
];

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Render the fly-status line above the input box (animated spinner verb or error).
pub fn render_fly_status(state: &AppState, frame: &mut Frame, area: Rect) {
    match &state.agent_status {
        AgentStatus::Streaming => {
            let spinner_char = SPINNER_FRAMES[state.tick_count % SPINNER_FRAMES.len()];
            // Rotate verb every ~40 ticks (~4 seconds at 100ms tick)
            let verb_idx = (state.tick_count / 40) % SPINNER_VERBS.len();
            let verb = SPINNER_VERBS[verb_idx];

            let line = ratatui::text::Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(spinner_char, Style::default().fg(Color::Cyan)),
                Span::styled(format!(" {}…", verb), Style::default().fg(Color::DarkGray)),
            ]);
            frame.render_widget(Paragraph::new(line), area);
        }
        AgentStatus::Error(_) => {
            let line = ratatui::text::Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled("Error occurred", Style::default().fg(Color::Red)),
            ]);
            frame.render_widget(Paragraph::new(line), area);
        }
        AgentStatus::Idle => {}
    }
}

/// Render the input bar.
pub fn render_input(state: &mut AppState, frame: &mut Frame, area: Rect) {
    let border_color = match &state.agent_status {
        AgentStatus::Idle => Color::Cyan,
        AgentStatus::Streaming => Color::Yellow,
        AgentStatus::Error(_) => Color::Red,
    };

    let prompt_color = match &state.agent_status {
        AgentStatus::Streaming => Color::DarkGray,
        _ => border_color,
    };

    // Top+bottom border with rounded corners, no left/right
    let block = Block::new()
        .borders(Borders::TOP | Borders::BOTTOM)
        .border_set(border::ROUNDED)
        .border_style(Style::default().fg(border_color));

    state.input.set_block(block);

    // Reserve 2 columns on the left for the ❯ prompt character
    let prompt_width: u16 = 2;
    let textarea_area = Rect::new(
        area.x + prompt_width,
        area.y,
        area.width.saturating_sub(prompt_width),
        area.height,
    );

    frame.render_widget(&state.input, textarea_area);

    // Render ❯ prompt character to the left of the textarea, below the top border
    if area.height > 1 {
        let prompt_area = Rect::new(area.x, area.y + 1, prompt_width, 1);
        let prompt_char = Paragraph::new(Span::styled(
            "❯ ",
            Style::default().fg(prompt_color),
        ));
        frame.render_widget(prompt_char, prompt_area);
    }

    // Draw the top and bottom borders across the full width (fill the prompt area gap)
    // The textarea's block only covers from prompt_width onward, so draw the left portion
    if area.width >= prompt_width {
        let border_line = "─".repeat(prompt_width as usize);
        let border_style = Style::default().fg(border_color);
        // Top
        let left_top = Paragraph::new(Span::styled(border_line.clone(), border_style));
        frame.render_widget(left_top, Rect::new(area.x, area.y, prompt_width, 1));
        // Bottom
        let left_bottom = Paragraph::new(Span::styled(border_line, border_style));
        frame.render_widget(left_bottom, Rect::new(area.x, area.y + area.height - 1, prompt_width, 1));
    }

    // Render inline argument hint
    render_argument_hint(state, frame, area, prompt_width);
}

/// Render a dimmed argument hint inline after the cursor when typing a slash command.
fn render_argument_hint(state: &AppState, frame: &mut Frame, area: Rect, prompt_width: u16) {
    let text = state.input.lines().join("\n");
    let trimmed = text.trim_start();

    if !trimmed.starts_with('/') {
        return;
    }

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

    // Position hint after input text. Inner area starts at prompt_width (no left border).
    let inner_x = area.x + prompt_width;
    let inner_width = area.width.saturating_sub(prompt_width);
    let text_len = text.len() as u16;

    if text_len >= inner_width {
        return;
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
    state.input.set_placeholder_text(" ");
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
