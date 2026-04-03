//! Fullscreen TUI for Strands CLI — inspired by Claude Code's terminal UI.

pub mod app;
pub mod event;
pub mod render;
pub mod terminal;
pub mod widgets;

use crossterm::event::{KeyCode, KeyModifiers};
use strands::Agent;

use self::app::{AgentStatus, TuiApp};
use self::event::Event;
use self::terminal::Tui;
use self::widgets::input_bar::{self, InputAction};

/// Run the fullscreen TUI.
pub async fn run(agent: Agent, model_name: String) -> strands::Result<()> {
    // Install panic hook to restore terminal on panic
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(
            std::io::stderr(),
            crossterm::terminal::LeaveAlternateScreen,
            crossterm::cursor::Show
        );
        original_hook(panic_info);
    }));

    let mut tui = Tui::new(12.0, 30.0).map_err(|e| strands::Error::Configuration(e.to_string()))?;
    tui.enter().map_err(|e| strands::Error::Configuration(e.to_string()))?;

    let mut app = TuiApp::new(agent, model_name);
    let mut event_rx = tui.event_rx.take().unwrap();
    let event_tx = tui.event_tx.clone();

    loop {
        let Some(event) = event_rx.recv().await else {
            break;
        };

        match event {
            Event::Render => {
                tui.terminal
                    .draw(|frame| render::view(frame, &mut app.state))
                    .map_err(|e| strands::Error::Configuration(e.to_string()))?;
            }
            Event::Tick => {
                app.state.tick_count = app.state.tick_count.wrapping_add(1);
            }
            Event::Key(key) => {
                handle_key(&mut app, key, event_tx.clone());
            }
            Event::Paste(text) => {
                // Insert pasted text into input
                for ch in text.chars() {
                    if ch == '\n' {
                        app.state.input.insert_newline();
                    } else {
                        app.state.input.insert_char(ch);
                    }
                }
            }
            Event::Resize(_, _) => {
                // Ratatui handles resize on next draw automatically
            }
            // Agent events
            Event::AgentTextDelta(_)
            | Event::AgentToolStart { .. }
            | Event::AgentToolCall { .. }
            | Event::AgentToolResult { .. }
            | Event::AgentDone
            | Event::AgentError(_) => {
                app.handle_agent_event(event);
            }
            _ => {}
        }

        if app.state.should_quit {
            break;
        }
    }

    tui.exit().map_err(|e| strands::Error::Configuration(e.to_string()))?;
    Ok(())
}

fn handle_key(
    app: &mut TuiApp,
    key: crossterm::event::KeyEvent,
    event_tx: tokio::sync::mpsc::UnboundedSender<Event>,
) {
    match (key.modifiers, key.code) {
        // Quit
        (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
            if matches!(app.state.agent_status, AgentStatus::Streaming) {
                // TODO: cancel the streaming task
                app.state.agent_status = AgentStatus::Idle;
            } else {
                app.state.should_quit = true;
            }
        }

        // Escape — cancel streaming or clear input
        (KeyModifiers::NONE, KeyCode::Esc) => {
            if matches!(app.state.agent_status, AgentStatus::Streaming) {
                app.state.agent_status = AgentStatus::Idle;
            }
        }

        // Scroll
        (KeyModifiers::NONE, KeyCode::PageUp) => {
            app.state.auto_scroll = false;
            app.state.scroll_offset = app.state.scroll_offset.saturating_add(10);
        }
        (KeyModifiers::NONE, KeyCode::PageDown) => {
            if app.state.scroll_offset <= 10 {
                app.state.scroll_offset = 0;
                app.state.auto_scroll = true;
            } else {
                app.state.scroll_offset = app.state.scroll_offset.saturating_sub(10);
            }
        }

        // Mouse scroll via arrow keys with shift
        (KeyModifiers::SHIFT, KeyCode::Up) => {
            app.state.auto_scroll = false;
            app.state.scroll_offset = app.state.scroll_offset.saturating_add(1);
        }
        (KeyModifiers::SHIFT, KeyCode::Down) => {
            if app.state.scroll_offset <= 1 {
                app.state.scroll_offset = 0;
                app.state.auto_scroll = true;
            } else {
                app.state.scroll_offset = app.state.scroll_offset.saturating_sub(1);
            }
        }

        // All other keys go to input bar (only when idle)
        _ => {
            if matches!(app.state.agent_status, AgentStatus::Idle | AgentStatus::Error(_)) {
                match input_bar::handle_input_key(&mut app.state, key) {
                    InputAction::Submit => {
                        app.submit(event_tx);
                    }
                    InputAction::Consumed => {}
                }
            }
        }
    }
}
