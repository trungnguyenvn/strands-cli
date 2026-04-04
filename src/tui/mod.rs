//! Fullscreen TUI for Strands CLI — inspired by Claude Code's terminal UI.

pub mod app;
pub mod event;
pub mod render;
pub mod terminal;
pub mod widgets;

#[cfg(test)]
mod tui_tests;

use crossterm::event::{KeyCode, KeyModifiers};
use strands::Agent;

use self::app::{AgentStatus, TuiApp};
use self::event::Event;
use self::terminal::Tui;
use self::widgets::input_bar::{self, InputAction};

/// Run the fullscreen TUI.
pub async fn run(agent: Agent, model_name: String, command_registry: crate::commands::CommandRegistry) -> strands::Result<()> {
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

    let mut app = TuiApp::new(agent, model_name, command_registry);
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
            Event::Resize(w, _) => {
                app.state.terminal_width = w;
                app.state.total_lines = 0; // force recompute on next render
            }
            // Mouse events
            Event::Mouse(mouse_event) => {
                handle_mouse(&mut app, mouse_event);
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

fn handle_mouse(app: &mut TuiApp, mouse: crossterm::event::MouseEvent) {
    use crossterm::event::MouseEventKind;
    match mouse.kind {
        MouseEventKind::ScrollUp => {
            app.state.auto_scroll = false;
            app.state.scroll_offset = app.state.scroll_offset.saturating_add(3);
        }
        MouseEventKind::ScrollDown => {
            if app.state.scroll_offset <= 3 {
                app.state.scroll_offset = 0;
                app.state.auto_scroll = true;
            } else {
                app.state.scroll_offset = app.state.scroll_offset.saturating_sub(3);
            }
        }
        _ => {}
    }
}

fn handle_key(
    app: &mut TuiApp,
    key: crossterm::event::KeyEvent,
    event_tx: tokio::sync::mpsc::UnboundedSender<Event>,
) {
    match (key.modifiers, key.code) {
        // Quit or cancel streaming
        (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
            if matches!(app.state.agent_status, AgentStatus::Streaming) {
                if let Some(ref a) = app.state.cancel_agent {
                    a.cancel();
                }
                app.state.agent_status = AgentStatus::Idle;
                app.state.cancel_agent = None;
            } else {
                app.state.should_quit = true;
            }
        }

        // Escape — dismiss suggestions or cancel streaming
        (KeyModifiers::NONE, KeyCode::Esc) => {
            if !app.state.suggestions.is_empty() {
                // Dismiss autocomplete dropdown (mirrors Claude Code's autocomplete:dismiss)
                app.state.suggestions.clear();
                app.state.selected_suggestion = -1;
            } else if matches!(app.state.agent_status, AgentStatus::Streaming) {
                if let Some(ref a) = app.state.cancel_agent {
                    a.cancel();
                }
                app.state.agent_status = AgentStatus::Idle;
                app.state.cancel_agent = None;
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

        // Tab — accept autocomplete suggestion (mirrors Claude Code's autocomplete:accept)
        (KeyModifiers::NONE, KeyCode::Tab) => {
            if !app.state.suggestions.is_empty() {
                app.accept_suggestion();
                // Re-trigger suggestions for the new input (e.g., "/clear " → no suggestions)
                app.update_suggestions();
            }
        }

        // All other keys go to input bar.
        // When streaming, only allow immediate slash commands (mirrors Claude Code's
        // handlePromptSubmit fast-path for `immediate: true` local-jsx commands).
        _ => {
            let is_idle = matches!(app.state.agent_status, AgentStatus::Idle | AgentStatus::Error(_));
            let is_streaming = matches!(app.state.agent_status, AgentStatus::Streaming);
            let has_suggestions = !app.state.suggestions.is_empty();

            if is_idle || is_streaming {
                // When suggestions are visible, intercept Up/Down for navigation
                // (mirrors Claude Code's autocomplete:previous / autocomplete:next)
                if has_suggestions {
                    match key.code {
                        KeyCode::Up => {
                            let len = app.state.suggestions.len() as i32;
                            app.state.selected_suggestion = if app.state.selected_suggestion <= 0 {
                                len - 1 // wrap to bottom
                            } else {
                                app.state.selected_suggestion - 1
                            };
                            return;
                        }
                        KeyCode::Down => {
                            let len = app.state.suggestions.len() as i32;
                            app.state.selected_suggestion =
                                if app.state.selected_suggestion >= len - 1 {
                                    0 // wrap to top
                                } else {
                                    app.state.selected_suggestion + 1
                                };
                            return;
                        }
                        _ => {}
                    }
                }

                match input_bar::handle_input_key(&mut app.state, key) {
                    InputAction::Submit => {
                        if has_suggestions && app.state.selected_suggestion >= 0 {
                            // Enter with suggestions visible: accept and submit if no args needed
                            // (mirrors Claude Code's handleEnter → applyCommandSuggestion with shouldExecute=true)
                            app.accept_suggestion();
                            app.submit(event_tx);
                        } else if is_idle {
                            app.submit(event_tx);
                        } else if is_streaming {
                            app.try_immediate_command();
                        }
                    }
                    InputAction::Consumed => {
                        // After every keystroke, update suggestions
                        if is_idle {
                            app.update_suggestions();
                        }
                    }
                }
            }
        }
    }
}
