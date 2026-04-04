//! Fullscreen TUI for Strands CLI — inspired by Claude Code's terminal UI.

pub mod app;
pub mod event;
pub mod keybindings;
pub mod render;
pub mod terminal;
pub mod widgets;

#[cfg(test)]
mod tui_tests;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crossterm::event::{KeyCode, KeyModifiers};
use strands::Agent;

use self::app::{AgentStatus, TuiApp, VimMode};
use self::event::Event;
use self::terminal::Tui;
use self::widgets::input_bar::{self, InputAction};

/// Run the fullscreen TUI.
/// Extra context data for the /context command, passed from main.
pub struct ContextSetup {
    pub system_prompt: String,
    pub tool_specs: Vec<crate::context::ToolSpecSummary>,
    pub memory_files: Vec<(String, String, String)>,
    pub skills: Vec<crate::context::SkillSummary>,
}

pub async fn run(agent: Agent, model_name: String, command_registry: crate::commands::CommandRegistry, cwd: PathBuf, context_setup: ContextSetup, session_id: Option<String>, session_title: Option<String>, model: std::sync::Arc<dyn strands::types::models::Model>) -> strands::Result<()> {
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

    let fullscreen = std::env::var("STRANDS_NO_FULLSCREEN").map(|v| v != "1").unwrap_or(true);
    let mut tui = Tui::new(12.0, 30.0).map_err(|e| strands::Error::Configuration(e.to_string()))?;
    tui.enter_with_fullscreen(fullscreen).map_err(|e| strands::Error::Configuration(e.to_string()))?;

    let mut app = TuiApp::new(agent, model_name, command_registry, model);
    app.state.session_id = session_id.clone();
    app.state.session_title = session_title;
    // Initialize file history for rewind support
    if let Some(ref sid) = session_id {
        strands_tools::file::file_history::init(sid);
    }
    app.state.system_prompt_text = context_setup.system_prompt;
    app.state.tool_spec_summaries = context_setup.tool_specs;
    app.state.memory_files = context_setup.memory_files;
    app.state.skill_summaries = context_setup.skills;
    let mut event_rx = tui.event_rx.take().unwrap();
    let event_tx = tui.event_tx.clone();

    // Load MCP servers in background — TUI is already visible
    let mcp_slot: Arc<Mutex<Option<crate::mcp::McpSession>>> = Arc::new(Mutex::new(None));
    {
        let slot = mcp_slot.clone();
        let tx = event_tx.clone();
        let cwd = cwd.clone();
        tokio::spawn(async move {
            let session = crate::mcp::load_mcp_servers(&cwd, true).await;
            *slot.lock().unwrap() = Some(session);
            let _ = tx.send(Event::McpLoaded);
        });
    }

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
                // Expire MCP warning after timeout
                if let app::McpStatus::Warning { expire_tick, .. } = app.state.mcp_status {
                    if app.state.tick_count >= expire_tick {
                        app.state.mcp_status = app::McpStatus::None;
                    }
                }
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
            // MCP servers finished loading in background
            Event::McpLoaded => {
                if let Some(session) = mcp_slot.lock().unwrap().take() {
                    app.apply_mcp_session(session);
                }
            }
            // Session title events
            Event::AiTitleGenerated(ref title) => {
                // Only set AI title if no custom title exists (custom always wins)
                if app.state.session_title.is_none() {
                    app.state.session_title = Some(title.clone());
                    if let Some(journal) = crate::session::get_journal() {
                        let journal = std::sync::Arc::clone(journal);
                        let t = title.clone();
                        tokio::spawn(async move {
                            let _ = journal.set_ai_title(t).await;
                        });
                    }
                }
            }
            Event::SessionTitleLoaded(title) => {
                app.state.session_title = Some(title);
            }
            // Session resume — rebuild display list from SDK messages
            Event::SessionResumed { messages, .. } => {
                // Clear the "/resume ..." placeholder messages
                app.state.messages.clear();
                app.state.clear_render_caches();
                // Convert SDK messages to display ChatMessages
                for msg in &messages {
                    let chat_msg = app::ChatMessage::from_sdk_message(msg);
                    // Skip empty messages (e.g. tool results with no text)
                    if !chat_msg.blocks.is_empty() {
                        app.state.messages.push(chat_msg);
                    }
                }
                // Update turn count to reflect resumed history
                app.state.turn_count = app.state.messages.iter()
                    .filter(|m| matches!(m.role, app::Role::User))
                    .count();
                app.state.auto_scroll = true;
                app.state.scroll_offset = 0;
            }
            // Agent events
            Event::AgentTextDelta(_)
            | Event::AgentToolStart { .. }
            | Event::AgentToolCall { .. }
            | Event::AgentToolResult { .. }
            | Event::AgentDone
            | Event::AgentError(_)
            | Event::PlanModeExited => {
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
            app.state.selection = Default::default(); // clear selection on scroll
            app.state.auto_scroll = false;
            app.state.scroll_offset = app.state.scroll_offset.saturating_add(3);
        }
        MouseEventKind::ScrollDown => {
            app.state.selection = Default::default(); // clear selection on scroll
            if app.state.scroll_offset <= 3 {
                app.state.scroll_offset = 0;
                app.state.auto_scroll = true;
                // Clear unseen divider when scrolling back to bottom
                app.state.unseen_from_line = None;
                app.state.unseen_message_count = 0;
            } else {
                app.state.scroll_offset = app.state.scroll_offset.saturating_sub(3);
            }
        }
        MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
            let area = app.state.selection.messages_area;
            if mouse.row >= area.y && mouse.row < area.y + area.height
                && mouse.column >= area.x && mouse.column < area.x + area.width
            {
                app.state.selection.active = true;
                app.state.selection.anchor = (mouse.row, mouse.column);
                app.state.selection.end = (mouse.row, mouse.column);
            }
        }
        MouseEventKind::Drag(crossterm::event::MouseButton::Left) => {
            if app.state.selection.active {
                app.state.selection.end = (mouse.row, mouse.column);
            }
        }
        MouseEventKind::Up(crossterm::event::MouseButton::Left) => {
            if app.state.selection.active {
                app.state.selection.end = (mouse.row, mouse.column);
                app.state.selection.active = false;

                let text = app.state.selection.selected_text();
                if !text.is_empty() {
                    copy_to_clipboard_osc52(&text);
                }
            }
        }
        _ => {}
    }
}

/// Copy text to clipboard using OSC 52 escape sequence.
/// Works in most modern terminals including over SSH.
fn copy_to_clipboard_osc52(text: &str) {
    use std::io::Write;
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    // Write OSC 52 to stderr (where the terminal is)
    let _ = write!(std::io::stderr(), "\x1b]52;c;{}\x07", encoded);
    let _ = std::io::stderr().flush();
}

/// Handle Ctrl+C: cancel streaming on first press, quit on double-press.
/// Tick rate is 12 Hz, so 24 ticks ≈ 2 seconds.
const CTRL_C_DOUBLE_TAP_TICKS: usize = 24;

fn handle_ctrl_c(app: &mut TuiApp) {
    // If streaming, first Ctrl+C always cancels the agent
    if matches!(app.state.agent_status, AgentStatus::Streaming) {
        if let Some(ref a) = app.state.cancel_agent {
            a.cancel();
        }
        app.state.agent_status = AgentStatus::Idle;
        app.state.cancel_agent = None;
        app.state.last_ctrl_c_tick = Some(app.state.tick_count);
        return;
    }

    // If input has text, first Ctrl+C clears it
    let has_input = !app.state.input.lines().join("").trim().is_empty();
    if has_input {
        app.reset_input();
        app.state.last_ctrl_c_tick = Some(app.state.tick_count);
        return;
    }

    // Double Ctrl+C within the window → quit
    if let Some(last_tick) = app.state.last_ctrl_c_tick {
        if app.state.tick_count.wrapping_sub(last_tick) <= CTRL_C_DOUBLE_TAP_TICKS {
            app.state.should_quit = true;
            return;
        }
    }

    // First Ctrl+C when idle with empty input → record and show hint
    app.state.last_ctrl_c_tick = Some(app.state.tick_count);
}

fn handle_key(
    app: &mut TuiApp,
    key: crossterm::event::KeyEvent,
    event_tx: tokio::sync::mpsc::UnboundedSender<Event>,
) {
    // --- Permission request overlay intercepts all keys ---
    if app.state.permission_request.is_some() {
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Char('y')) => {
                if let Some(ref mut req) = app.state.permission_request {
                    req.decision = Some(true);
                }
                app.state.permission_request = None;
            }
            (KeyModifiers::NONE, KeyCode::Char('n')) => {
                if let Some(ref mut req) = app.state.permission_request {
                    req.decision = Some(false);
                }
                app.state.permission_request = None;
            }
            (KeyModifiers::NONE, KeyCode::Char('a')) => {
                // "Always allow" — allow + could store preference
                if let Some(ref mut req) = app.state.permission_request {
                    req.decision = Some(true);
                }
                app.state.permission_request = None;
            }
            (KeyModifiers::NONE, KeyCode::Esc) => {
                // Deny on Esc
                if let Some(ref mut req) = app.state.permission_request {
                    req.decision = Some(false);
                }
                app.state.permission_request = None;
            }
            _ => {} // Ignore all other keys during permission prompt
        }
        return;
    }

    // --- Toggle vim mode (Ctrl+V) ---
    if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('v') {
        app.state.vim_mode = match app.state.vim_mode {
            VimMode::Off => VimMode::Normal,
            VimMode::Normal | VimMode::Insert => VimMode::Off,
        };
        return;
    }

    // --- Vim Normal mode key handling ---
    if app.state.vim_mode == VimMode::Normal {
        match (key.modifiers, key.code) {
            // Esc stays in Normal mode (no-op)
            (KeyModifiers::NONE, KeyCode::Esc) => {
                if !app.state.suggestions.is_empty() {
                    app.state.suggestions.clear();
                    app.state.selected_suggestion = -1;
                } else if matches!(app.state.agent_status, AgentStatus::Streaming) {
                    if let Some(ref a) = app.state.cancel_agent {
                        a.cancel();
                    }
                    app.state.agent_status = AgentStatus::Idle;
                    app.state.cancel_agent = None;
                }
                return;
            }
            (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
                handle_ctrl_c(app);
                return;
            }
            _ => {}
        }

        // Delegate to vim normal mode handler
        match input_bar::handle_vim_normal_key(&mut app.state, key) {
            InputAction::Submit => {
                let is_idle = matches!(app.state.agent_status, AgentStatus::Idle | AgentStatus::Error(_));
                let is_streaming = matches!(app.state.agent_status, AgentStatus::Streaming);
                if is_idle {
                    app.submit(event_tx);
                } else if is_streaming {
                    app.try_immediate_command();
                }
            }
            InputAction::Consumed => {}
        }
        return;
    }

    // --- Vim Insert mode: Esc returns to Normal ---
    if app.state.vim_mode == VimMode::Insert {
        if key.modifiers == KeyModifiers::NONE && key.code == KeyCode::Esc {
            app.state.vim_mode = VimMode::Normal;
            return;
        }
        // Fall through to normal key handling below
    }

    // --- Standard key handling (vim off or vim insert mode) ---
    match (key.modifiers, key.code) {
        // Quit or cancel streaming
        (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
            handle_ctrl_c(app);
        }

        // Escape — dismiss suggestions, cancel streaming, or double-tap rewind
        (KeyModifiers::NONE, KeyCode::Esc) => {
            if !app.state.suggestions.is_empty() {
                app.state.suggestions.clear();
                app.state.selected_suggestion = -1;
            } else if matches!(app.state.agent_status, AgentStatus::Streaming) {
                if let Some(ref a) = app.state.cancel_agent {
                    a.cancel();
                }
                app.state.agent_status = AgentStatus::Idle;
                app.state.cancel_agent = None;
            } else {
                // Double-tap Esc to open rewind (mirrors Claude Code)
                let has_input = !app.state.input.lines().join("").trim().is_empty();
                let is_idle = matches!(app.state.agent_status, AgentStatus::Idle | AgentStatus::Error(_));
                if !has_input && is_idle && !app.state.messages.is_empty() {
                    if let Some(last_tick) = app.state.last_esc_tick {
                        // 800ms window at 12Hz tick rate ≈ 10 ticks
                        if app.state.tick_count.wrapping_sub(last_tick) <= 10 {
                            app.set_input("/rewind ");
                            app.update_suggestions();
                            app.state.last_esc_tick = None;
                            return;
                        }
                    }
                    app.state.last_esc_tick = Some(app.state.tick_count);
                }
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
                // Clear unseen divider when scrolling back to bottom
                app.state.unseen_from_line = None;
                app.state.unseen_message_count = 0;
            } else {
                app.state.scroll_offset = app.state.scroll_offset.saturating_sub(10);
            }
        }

        // Shift+Arrow scroll
        (KeyModifiers::SHIFT, KeyCode::Up) => {
            app.state.auto_scroll = false;
            app.state.scroll_offset = app.state.scroll_offset.saturating_add(1);
        }
        (KeyModifiers::SHIFT, KeyCode::Down) => {
            if app.state.scroll_offset <= 1 {
                app.state.scroll_offset = 0;
                app.state.auto_scroll = true;
                app.state.unseen_from_line = None;
                app.state.unseen_message_count = 0;
            } else {
                app.state.scroll_offset = app.state.scroll_offset.saturating_sub(1);
            }
        }

        // Shift+Tab — cycle permission mode (matches Claude Code)
        (KeyModifiers::SHIFT, KeyCode::BackTab) => {
            let next_name = match app.state.permission_mode {
                app::PermissionMode::Default => "plan",
                app::PermissionMode::Plan => "accept-edits",
                app::PermissionMode::AcceptEdits => "bypass",
                app::PermissionMode::BypassPermissions => "default",
            };
            app.apply_mode_switch(next_name);
        }

        // Tab — accept autocomplete suggestion or typeahead
        (KeyModifiers::NONE, KeyCode::Tab) => {
            if !app.state.suggestions.is_empty() {
                app.accept_suggestion();
                app.update_suggestions();
            } else if let Some(prediction) = app.state.typeahead.take() {
                // Accept typeahead prediction
                for ch in prediction.chars() {
                    app.state.input.insert_char(ch);
                }
            }
        }

        // All other keys go to input bar
        _ => {
            let is_idle = matches!(app.state.agent_status, AgentStatus::Idle | AgentStatus::Error(_));
            let is_streaming = matches!(app.state.agent_status, AgentStatus::Streaming);
            let has_suggestions = !app.state.suggestions.is_empty();

            if is_idle || is_streaming {
                if has_suggestions {
                    match key.code {
                        KeyCode::Up => {
                            let len = app.state.suggestions.len() as i32;
                            app.state.selected_suggestion = if app.state.selected_suggestion <= 0 {
                                len - 1
                            } else {
                                app.state.selected_suggestion - 1
                            };
                            return;
                        }
                        KeyCode::Down => {
                            let len = app.state.suggestions.len() as i32;
                            app.state.selected_suggestion =
                                if app.state.selected_suggestion >= len - 1 {
                                    0
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
                            if let Some(model_id) = app.selected_model_id() {
                                app.reset_input();
                                app.switch_model(model_id, event_tx.clone());
                            } else if let Some(session_id) = app.selected_session_id() {
                                app.reset_input();
                                app.resume_session(session_id, event_tx.clone());
                            } else if let Some((msg_idx, msg_id)) = app.selected_rewind_info() {
                                app.reset_input();
                                app.rewind_to(msg_idx, &msg_id);
                            } else {
                                app.accept_suggestion();
                                app.submit(event_tx);
                            }
                        } else if is_idle {
                            app.submit(event_tx);
                        } else if is_streaming {
                            app.try_immediate_command();
                        }
                    }
                    InputAction::Consumed => {
                        // Clear typeahead on any keystroke
                        app.state.typeahead = None;
                        // Update suggestions
                        if is_idle {
                            app.update_suggestions();
                            // Generate simple typeahead predictions
                            update_typeahead(&mut app.state);
                        }
                    }
                }
            }
        }
    }
}


/// Generate simple typeahead predictions based on input history.
/// Mirrors Claude Code's speculation/promptSuggestion infrastructure.
fn update_typeahead(state: &mut app::AppState) {
    let text = state.input.lines().join("\n");
    let trimmed = text.trim();

    if trimmed.is_empty() || trimmed.starts_with('/') {
        state.typeahead = None;
        return;
    }

    // Simple prefix match against input history
    for hist in state.input_history.iter().rev() {
        if hist.starts_with(trimmed) && hist.len() > trimmed.len() {
            state.typeahead = Some(hist[trimmed.len()..].to_string());
            return;
        }
    }

    state.typeahead = None;
}
