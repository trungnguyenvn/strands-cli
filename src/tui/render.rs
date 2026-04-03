//! Layout assembly and main view function.

use ratatui::layout::{Constraint, Layout};
use ratatui::Frame;

use super::app::AppState;
use super::widgets::{input_bar, messages, status_bar};

pub fn view(frame: &mut Frame, state: &mut AppState) {
    let terminal_height = frame.area().height;
    let input_h = input_bar::input_height(state, terminal_height);

    let chunks = Layout::vertical([
        Constraint::Fill(1),         // message history
        Constraint::Length(input_h), // input bar
        Constraint::Length(1),       // status bar
    ])
    .split(frame.area());

    messages::render_messages(state, frame, chunks[0]);
    input_bar::render_input(state, frame, chunks[1]);
    status_bar::render_status_bar(state, frame, chunks[2]);
}
