//! Layout assembly and main view function.

use ratatui::layout::{Constraint, Layout};
use ratatui::Frame;

use super::app::AppState;
use super::widgets::{input_bar, messages, status_bar, suggestions};

pub fn view(frame: &mut Frame, state: &mut AppState) {
    let terminal_height = frame.area().height;
    let input_h = input_bar::input_height(state, terminal_height);

    // Reserve space for suggestion dropdown between messages and input
    let suggestion_count = state.suggestions.len().min(6) as u16;
    let suggestion_h = if suggestion_count > 0 {
        suggestion_count
    } else {
        0
    };

    let chunks = Layout::vertical([
        Constraint::Fill(1),                    // message history
        Constraint::Length(suggestion_h),        // autocomplete dropdown
        Constraint::Length(input_h),             // input bar
        Constraint::Length(1),                   // status bar
    ])
    .split(frame.area());

    messages::render_messages(state, frame, chunks[0]);
    if suggestion_h > 0 {
        suggestions::render_suggestions(
            &state.suggestions,
            state.selected_suggestion,
            frame,
            chunks[1],
        );
    }
    input_bar::render_input(state, frame, chunks[2]);
    status_bar::render_status_bar(state, frame, chunks[3]);

}
