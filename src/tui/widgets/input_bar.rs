//! Input bar widget — wraps tui-textarea with submit/newline key handling.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::Block;
use ratatui::Frame;

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

        // Forward everything else to textarea
        _ => {
            state
                .input
                .input(crossterm::event::Event::Key(key));
            InputAction::Consumed
        }
    }
}

pub enum InputAction {
    Submit,
    Consumed,
}
