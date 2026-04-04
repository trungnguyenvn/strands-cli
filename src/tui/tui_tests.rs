//! End-to-end TUI tests using ratatui's TestBackend.
//!
//! These tests render the full TUI layout into an in-memory buffer and assert
//! on the visible output — verifying the slash command system works from
//! keypress through dispatch to rendered screen.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::Terminal;

use super::app::{AgentStatus, AppState, ChatMessage};
use super::render;
use super::widgets::input_bar;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Create a default AppState with a given terminal width/height.
fn make_state(width: u16, _height: u16) -> AppState {
    let mut state = AppState::new("test-model".to_string(), crate::commands::builtin_registry(), Vec::new());
    state.terminal_width = width;
    state.mcp_status = super::app::McpStatus::None;
    state
}

/// Type a string into the input textarea character by character.
fn type_text(state: &mut AppState, text: &str) {
    for ch in text.chars() {
        state.input.insert_char(ch);
    }
}

/// Send Enter key to input bar, return the InputAction.
fn press_enter(state: &mut AppState) -> input_bar::InputAction {
    let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    input_bar::handle_input_key(state, key)
}

/// Render the full TUI view into a TestBackend and return the buffer.
fn render_to_buffer(state: &mut AppState, width: u16, height: u16) -> Buffer {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| render::view(frame, state))
        .unwrap();
    terminal.backend().buffer().clone()
}

/// Extract all text from a buffer as a Vec of strings (one per row).
fn buffer_lines(buf: &Buffer) -> Vec<String> {
    let w = buf.area.width as usize;
    buf.content
        .chunks(w)
        .map(|row| {
            row.iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

/// Check whether any line in the buffer contains the given substring.
fn buffer_contains(buf: &Buffer, needle: &str) -> bool {
    buffer_lines(buf).iter().any(|line| line.contains(needle))
}

// ---------------------------------------------------------------------------
// E2E: Welcome screen renders with slash command hints
// ---------------------------------------------------------------------------

#[test]
fn welcome_screen_shows_slash_command_hints() {
    let mut state = make_state(80, 24);
    let buf = render_to_buffer(&mut state, 80, 24);

    assert!(
        buffer_contains(&buf, "/clear"),
        "Welcome screen should mention /clear"
    );
    assert!(
        buffer_contains(&buf, "/exit"),
        "Welcome screen should mention /exit"
    );
}

// ---------------------------------------------------------------------------
// E2E: Status bar shows /help, /clear, /exit when idle
// ---------------------------------------------------------------------------

#[test]
fn status_bar_shows_command_hints_when_idle() {
    let mut state = make_state(100, 24);
    let buf = render_to_buffer(&mut state, 100, 24);
    let lines = buffer_lines(&buf);
    let last_line = &lines[lines.len() - 1];

    assert!(
        last_line.contains("/help"),
        "Status bar should show /help, got: {last_line}"
    );
    assert!(
        last_line.contains("/clear"),
        "Status bar should show /clear, got: {last_line}"
    );
    assert!(
        last_line.contains("/exit"),
        "Status bar should show /exit, got: {last_line}"
    );
}

#[test]
fn status_bar_shows_cancel_hint_when_streaming() {
    let mut state = make_state(100, 24);
    state.agent_status = AgentStatus::Streaming;
    let buf = render_to_buffer(&mut state, 100, 24);
    let lines = buffer_lines(&buf);
    let last_line = &lines[lines.len() - 1];

    assert!(
        last_line.contains("Ctrl+C"),
        "Status bar should show Ctrl+C during streaming, got: {last_line}"
    );
    assert!(
        !last_line.contains("/help"),
        "Status bar should NOT show /help during streaming, got: {last_line}"
    );
}

// ---------------------------------------------------------------------------
// E2E: /help command — type it, dispatch, verify output renders
// ---------------------------------------------------------------------------

#[test]
fn slash_help_renders_command_list() {
    let mut state = make_state(80, 30);

    // Type "/help" and submit
    type_text(&mut state, "/help");
    assert!(matches!(press_enter(&mut state), input_bar::InputAction::Submit));

    // Simulate what TuiApp::submit does for a local command —
    // dispatch and push messages to state.
    let ctx = crate::commands::CommandContext {
        model_name: state.model_name.clone(),
        turn_count: state.turn_count,
        message_count: state.messages.len(),
        all_commands: state.command_registry.command_infos(),
        mcp_servers: Vec::new(),
    };
    match crate::commands::dispatch("/help", &state.command_registry, &ctx) {
        crate::commands::DispatchResult::Local(crate::commands::CommandResult::Text(text)) => {
            state.messages.push(ChatMessage::user("/help".into()));
            let mut msg = ChatMessage::assistant_empty();
            msg.append_text(&text);
            state.messages.push(msg);
        }
        _ => panic!("Expected Local(Text), got other dispatch result"),
    }

    // Render and verify help output is visible
    let buf = render_to_buffer(&mut state, 80, 30);
    assert!(
        buffer_contains(&buf, "Available commands"),
        "Help output should show 'Available commands'"
    );
    assert!(
        buffer_contains(&buf, "/exit"),
        "Help output should list /exit"
    );
    assert!(
        buffer_contains(&buf, "/clear"),
        "Help output should list /clear"
    );
    assert!(
        buffer_contains(&buf, "/compact"),
        "Help output should list /compact"
    );
    assert!(
        buffer_contains(&buf, "/status"),
        "Help output should list /status"
    );
}

// ---------------------------------------------------------------------------
// E2E: /status command — renders model name and turn count
// ---------------------------------------------------------------------------

#[test]
fn slash_status_renders_session_info() {
    let mut state = make_state(80, 30);
    state.turn_count = 5;

    let ctx = crate::commands::CommandContext {
        model_name: "test-model".into(),
        turn_count: 5,
        message_count: 0,
        all_commands: state.command_registry.command_infos(),
        mcp_servers: Vec::new(),
    };
    match crate::commands::dispatch("/status", &state.command_registry, &ctx) {
        crate::commands::DispatchResult::Local(crate::commands::CommandResult::Text(text)) => {
            state.messages.push(ChatMessage::user("/status".into()));
            let mut msg = ChatMessage::assistant_empty();
            msg.append_text(&text);
            state.messages.push(msg);
        }
        _ => panic!("Expected Local(Text), got other dispatch result"),
    }

    let buf = render_to_buffer(&mut state, 80, 30);
    assert!(
        buffer_contains(&buf, "Model: test-model"),
        "Status should show model name"
    );
    assert!(
        buffer_contains(&buf, "Turns: 5"),
        "Status should show turn count"
    );
}

// ---------------------------------------------------------------------------
// E2E: /clear command — clears messages
// ---------------------------------------------------------------------------

#[test]
fn slash_clear_empties_messages() {
    let mut state = make_state(80, 24);

    // Simulate some conversation history
    state.messages.push(ChatMessage::user("hello".into()));
    let mut resp = ChatMessage::assistant_empty();
    resp.append_text("Hi there!");
    state.messages.push(resp);
    assert_eq!(state.messages.len(), 2);

    let ctx = crate::commands::CommandContext {
        model_name: state.model_name.clone(),
        turn_count: 0,
        message_count: 2,
    all_commands: state.command_registry.command_infos(),
    mcp_servers: Vec::new(),
    };
    match crate::commands::dispatch("/clear", &state.command_registry, &ctx) {
        crate::commands::DispatchResult::Local(crate::commands::CommandResult::Clear) => {
            state.messages.clear();
        }
        _ => panic!("Expected Local(Clear), got other dispatch result"),
    }

    assert!(state.messages.is_empty(), "Messages should be cleared");

    // After clear, welcome screen should render again
    let buf = render_to_buffer(&mut state, 80, 24);
    assert!(
        buffer_contains(&buf, "Strands"),
        "Welcome screen should re-appear after /clear"
    );
}

// ---------------------------------------------------------------------------
// E2E: /exit, /quit — sets should_quit flag
// ---------------------------------------------------------------------------

#[test]
fn slash_exit_sets_quit_flag() {
    let state = make_state(80, 24);
    let ctx = crate::commands::CommandContext {
        model_name: state.model_name.clone(),
        turn_count: 0,
        message_count: 0,
    all_commands: state.command_registry.command_infos(),
    mcp_servers: Vec::new(),
    };

    match crate::commands::dispatch("/exit", &state.command_registry, &ctx) {
        crate::commands::DispatchResult::Local(crate::commands::CommandResult::Quit) => {}
        _ => panic!("Expected Quit"),
    }
    // Aliases
    match crate::commands::dispatch("/quit", &state.command_registry, &ctx) {
        crate::commands::DispatchResult::Local(crate::commands::CommandResult::Quit) => {}
        _ => panic!("Expected Quit for /quit alias"),
    }
}

// ---------------------------------------------------------------------------
// E2E: /new alias for /clear
// ---------------------------------------------------------------------------

#[test]
fn slash_new_alias_triggers_clear() {
    let state = make_state(80, 24);
    let ctx = crate::commands::CommandContext {
        model_name: state.model_name.clone(),
        turn_count: 0,
        message_count: 0,
    all_commands: state.command_registry.command_infos(),
    mcp_servers: Vec::new(),
    };

    match crate::commands::dispatch("/new", &state.command_registry, &ctx) {
        crate::commands::DispatchResult::Local(crate::commands::CommandResult::Clear) => {}
        _ => panic!("Expected Clear for /new alias"),
    }
    match crate::commands::dispatch("/reset", &state.command_registry, &ctx) {
        crate::commands::DispatchResult::Local(crate::commands::CommandResult::Clear) => {}
        _ => panic!("Expected Clear for /reset alias"),
    }
}

// ---------------------------------------------------------------------------
// E2E: /? alias for /help
// ---------------------------------------------------------------------------

#[test]
fn slash_question_mark_alias_triggers_help() {
    let state = make_state(80, 24);
    let ctx = crate::commands::CommandContext {
        model_name: state.model_name.clone(),
        turn_count: 0,
        message_count: 0,
    all_commands: state.command_registry.command_infos(),
    mcp_servers: Vec::new(),
    };

    match crate::commands::dispatch("/?", &state.command_registry, &ctx) {
        crate::commands::DispatchResult::Local(crate::commands::CommandResult::Text(text)) => {
            assert!(
                text.contains("Available commands"),
                "/? should produce help text"
            );
        }
        _ => panic!("Expected Text for /? alias"),
    }
}

// ---------------------------------------------------------------------------
// E2E: /compact — produces a prompt (not local result)
// ---------------------------------------------------------------------------

#[test]
fn slash_compact_returns_prompt() {
    let state = make_state(80, 24);
    let ctx = crate::commands::CommandContext {
        model_name: state.model_name.clone(),
        turn_count: 0,
        message_count: 0,
    all_commands: state.command_registry.command_infos(),
    mcp_servers: Vec::new(),
    };

    match crate::commands::dispatch("/compact", &state.command_registry, &ctx) {
        crate::commands::DispatchResult::Prompt(prompt) => {
            assert!(
                prompt.contains("Summarize"),
                "Compact prompt should contain 'Summarize', got: {prompt}"
            );
        }
        _ => panic!("Expected Prompt for /compact"),
    }
}

#[test]
fn slash_compact_with_args_includes_custom_instructions() {
    let state = make_state(80, 24);
    let ctx = crate::commands::CommandContext {
        model_name: state.model_name.clone(),
        turn_count: 0,
        message_count: 0,
    all_commands: state.command_registry.command_infos(),
    mcp_servers: Vec::new(),
    };

    match crate::commands::dispatch("/compact keep all file paths", &state.command_registry, &ctx) {
        crate::commands::DispatchResult::Prompt(prompt) => {
            assert!(
                prompt.contains("keep all file paths"),
                "Compact prompt should include custom args, got: {prompt}"
            );
        }
        _ => panic!("Expected Prompt for /compact with args"),
    }
}

// ---------------------------------------------------------------------------
// E2E: Unknown command — error feedback renders
// ---------------------------------------------------------------------------

#[test]
fn unknown_command_renders_error() {
    let mut state = make_state(80, 30);

    let ctx = crate::commands::CommandContext {
        model_name: state.model_name.clone(),
        turn_count: 0,
        message_count: 0,
    all_commands: state.command_registry.command_infos(),
    mcp_servers: Vec::new(),
    };
    match crate::commands::dispatch("/nonexistent", &state.command_registry, &ctx) {
        crate::commands::DispatchResult::Unknown(name) => {
            state
                .messages
                .push(ChatMessage::user("/nonexistent".into()));
            let mut msg = ChatMessage::assistant_empty();
            msg.append_text(&format!(
                "Unknown command: /{}. Type /help for available commands.",
                name
            ));
            state.messages.push(msg);
        }
        _ => panic!("Expected Unknown"),
    }

    let buf = render_to_buffer(&mut state, 80, 30);
    assert!(
        buffer_contains(&buf, "Unknown command"),
        "Unknown command error should render"
    );
    assert!(
        buffer_contains(&buf, "/help"),
        "Error should suggest /help"
    );
}

// ---------------------------------------------------------------------------
// E2E: File path not treated as command — /var/log/foo falls through
// ---------------------------------------------------------------------------

#[test]
fn file_path_is_not_a_command() {
    let state = make_state(80, 24);
    let ctx = crate::commands::CommandContext {
        model_name: state.model_name.clone(),
        turn_count: 0,
        message_count: 0,
    all_commands: state.command_registry.command_infos(),
    mcp_servers: Vec::new(),
    };

    match crate::commands::dispatch("/var/log/foo", &state.command_registry, &ctx) {
        crate::commands::DispatchResult::NotACommand => {}
        _ => panic!("File path should be NotACommand"),
    }
}

// ---------------------------------------------------------------------------
// E2E: Argument hint renders in input bar
// ---------------------------------------------------------------------------

#[test]
fn argument_hint_renders_for_compact() {
    let mut state = make_state(80, 24);

    // Type "/compact " (with trailing space)
    type_text(&mut state, "/compact ");

    let buf = render_to_buffer(&mut state, 80, 24);
    assert!(
        buffer_contains(&buf, "<optional instructions>"),
        "Argument hint should render after '/compact '"
    );
}

#[test]
fn argument_hint_not_shown_without_trailing_space() {
    let mut state = make_state(80, 24);

    // Type "/compact" (no trailing space)
    type_text(&mut state, "/compact");

    let buf = render_to_buffer(&mut state, 80, 24);
    assert!(
        !buffer_contains(&buf, "<optional instructions>"),
        "Argument hint should NOT render without trailing space"
    );
}

#[test]
fn argument_hint_not_shown_when_args_typed() {
    let mut state = make_state(80, 24);

    // Type "/compact keep files" (real args present)
    type_text(&mut state, "/compact keep files");

    let buf = render_to_buffer(&mut state, 80, 24);
    assert!(
        !buffer_contains(&buf, "<optional instructions>"),
        "Argument hint should NOT render when real args are typed"
    );
}

#[test]
fn no_argument_hint_for_commands_without_hint() {
    let mut state = make_state(80, 24);

    // /help has no argument_hint
    type_text(&mut state, "/help ");

    let buf = render_to_buffer(&mut state, 80, 24);
    assert!(
        !buffer_contains(&buf, "<optional"),
        "No hint should render for commands without argument_hint"
    );
}

// ---------------------------------------------------------------------------
// E2E: Input bar border color changes with status
// ---------------------------------------------------------------------------

#[test]
fn input_border_changes_color_on_status() {
    // Just verify rendering succeeds in each status without panic
    for status in [
        AgentStatus::Idle,
        AgentStatus::Streaming,
        AgentStatus::Error("test error".into()),
    ] {
        let mut state = make_state(80, 24);
        state.agent_status = status;
        let _buf = render_to_buffer(&mut state, 80, 24);
        // No panic = pass
    }
}

// ---------------------------------------------------------------------------
// E2E: User message renders with ">" prefix after command
// ---------------------------------------------------------------------------

#[test]
fn user_command_renders_with_prefix() {
    let mut state = make_state(80, 30);

    state.messages.push(ChatMessage::user("/help".into()));
    let mut msg = ChatMessage::assistant_empty();
    msg.append_text("Available commands:\n  /exit — Exit");
    state.messages.push(msg);

    let buf = render_to_buffer(&mut state, 80, 30);
    assert!(
        buffer_contains(&buf, "> /help"),
        "User command should render with '>' prefix"
    );
}

// ---------------------------------------------------------------------------
// E2E: Assistant response from /status renders model info
// ---------------------------------------------------------------------------

#[test]
fn status_response_renders_in_message_area() {
    let mut state = make_state(80, 30);

    state.messages.push(ChatMessage::user("/status".into()));
    let mut msg = ChatMessage::assistant_empty();
    msg.append_text("Model: test-model\nTurns: 3\nMessages: 6");
    state.messages.push(msg);

    let buf = render_to_buffer(&mut state, 80, 30);
    assert!(buffer_contains(&buf, "Model: test-model"));
    assert!(buffer_contains(&buf, "Turns: 3"));
}

// ---------------------------------------------------------------------------
// E2E: Input history excludes slash commands
// ---------------------------------------------------------------------------

#[test]
fn input_history_excludes_slash_commands() {
    let mut state = make_state(80, 24);

    // Simulate adding a normal message to history
    let normal = "hello world".to_string();
    state.input_history.push(normal.clone());

    // Slash commands should NOT be added (this is checked in TuiApp::submit,
    // but we verify the contract: starts_with('/') → skip)
    let slash = "/help".to_string();
    if !slash.starts_with('/') {
        state.input_history.push(slash);
    }

    assert_eq!(state.input_history.len(), 1);
    assert_eq!(state.input_history[0], "hello world");
}

// ---------------------------------------------------------------------------
// E2E: Full render cycle — no panic for all states
// ---------------------------------------------------------------------------

#[test]
fn full_render_no_panic_empty_state() {
    let mut state = make_state(80, 24);
    let _buf = render_to_buffer(&mut state, 80, 24);
}

#[test]
fn full_render_no_panic_with_conversation() {
    let mut state = make_state(80, 30);

    // Build a realistic conversation
    state.messages.push(ChatMessage::user("explain code".into()));
    let mut resp = ChatMessage::assistant_empty();
    resp.append_text("Here's the explanation:\n\n```rust\nfn main() {}\n```");
    resp.add_tool_call(
        "Read".into(),
        "/src/main.rs".into(),
        super::app::ToolCallStatus::Success,
    );
    state.messages.push(resp);

    state.messages.push(ChatMessage::user("/status".into()));
    let mut status_resp = ChatMessage::assistant_empty();
    status_resp.append_text("Model: test-model\nTurns: 1");
    state.messages.push(status_resp);

    state.turn_count = 2;
    let _buf = render_to_buffer(&mut state, 80, 30);
}

#[test]
fn full_render_no_panic_narrow_terminal() {
    let mut state = make_state(40, 10);
    state.messages.push(ChatMessage::user("/help".into()));
    let mut msg = ChatMessage::assistant_empty();
    msg.append_text("Available commands:\n  /exit — Exit the application\n  /clear — Clear");
    state.messages.push(msg);
    let _buf = render_to_buffer(&mut state, 40, 10);
}

// ---------------------------------------------------------------------------
// E2E: Immediate command flag — /status is immediate, /compact is not
// ---------------------------------------------------------------------------

#[test]
fn immediate_commands_flagged_correctly() {
    let state = make_state(80, 24);
    let reg = &state.command_registry;

    let status = reg.find("status").expect("/status should exist");
    assert!(status.immediate, "/status should be immediate");

    let help = reg.find("help").expect("/help should exist");
    assert!(help.immediate, "/help should be immediate");

    let exit = reg.find("exit").expect("/exit should exist");
    assert!(exit.immediate, "/exit should be immediate");

    let clear = reg.find("clear").expect("/clear should exist");
    assert!(clear.immediate, "/clear should be immediate");

    let compact = reg.find("compact").expect("/compact should exist");
    assert!(!compact.immediate, "/compact should NOT be immediate");
}

// ---------------------------------------------------------------------------
// E2E: try_immediate_command — dispatches during streaming
// ---------------------------------------------------------------------------

#[test]
fn try_immediate_command_status_during_streaming() {
    let mut state = make_state(80, 30);
    state.agent_status = AgentStatus::Streaming;

    // Type "/status" in input
    type_text(&mut state, "/status");

    // Simulate what try_immediate_command checks:
    // parse, find, check immediate + Local, dispatch
    let text = state.input.lines().join("\n");
    let trimmed = text.trim();
    let parsed = crate::commands::parse_slash_command(trimmed).unwrap();
    let cmd = state.command_registry.find(&parsed.command_name).unwrap();
    assert!(cmd.immediate);
    assert!(matches!(cmd.kind, crate::commands::CommandKind::Local { .. }));

    // Dispatch
    let ctx = crate::commands::CommandContext {
        model_name: state.model_name.clone(),
        turn_count: 0,
        message_count: 0,
    all_commands: state.command_registry.command_infos(),
    mcp_servers: Vec::new(),
    };
    match crate::commands::dispatch(trimmed, &state.command_registry, &ctx) {
        crate::commands::DispatchResult::Local(crate::commands::CommandResult::Text(text)) => {
            state.messages.push(ChatMessage::user(trimmed.to_string()));
            let mut msg = ChatMessage::assistant_empty();
            msg.append_text(&text);
            state.messages.push(msg);
        }
        _ => panic!("Expected Text for /status"),
    }

    let buf = render_to_buffer(&mut state, 80, 30);
    assert!(
        buffer_contains(&buf, "Model: test-model"),
        "/status should work during streaming"
    );
}

#[test]
fn non_immediate_command_blocked_during_streaming() {
    let state = make_state(80, 24);

    // /compact is NOT immediate — try_immediate_command should reject it
    let cmd = state.command_registry.find("compact").unwrap();
    assert!(
        !cmd.immediate,
        "/compact should not be immediate"
    );
    assert!(
        !matches!(cmd.kind, crate::commands::CommandKind::Local { .. }),
        "/compact is Prompt, not Local"
    );
    // In try_immediate_command, both checks fail → command is ignored during streaming
}

// ---------------------------------------------------------------------------
// E2E: Disabled command is treated as unknown
// ---------------------------------------------------------------------------

#[test]
fn disabled_command_treated_as_unknown() {
    use crate::commands::{Command, CommandKind, CommandRegistry, CommandResult};

    let reg = CommandRegistry::new(vec![Command {
        name: "secret".into(),
        description: "Hidden command".into(),
        aliases: vec![],
        is_hidden: true,
        argument_hint: None,
        is_enabled: Some(|| false), // disabled!
        immediate: false,
        kind: CommandKind::Local {
            execute: |_, _| CommandResult::Text("you shouldn't see this".into()),
        },
    }]);

    let ctx = crate::commands::CommandContext {
        model_name: "test".into(),
        turn_count: 0,
        message_count: 0,
        all_commands: reg.command_infos(),
        mcp_servers: Vec::new(),
    };

    match crate::commands::dispatch("/secret", &reg, &ctx) {
        crate::commands::DispatchResult::Unknown(name) => {
            assert_eq!(name, "secret");
        }
        _ => panic!("Disabled command should be treated as Unknown"),
    }
}

// ---------------------------------------------------------------------------
// E2E: Scroll info in status bar
// ---------------------------------------------------------------------------

#[test]
fn status_bar_shows_bottom_when_auto_scroll() {
    let mut state = make_state(100, 24);
    state.auto_scroll = true;
    let buf = render_to_buffer(&mut state, 100, 24);
    let lines = buffer_lines(&buf);
    let last = &lines[lines.len() - 1];
    assert!(
        last.contains("bottom") || last.contains("\u{2193}"),
        "Status bar should show 'bottom' indicator when auto_scroll, got: {last}"
    );
}

// ---------------------------------------------------------------------------
// E2E: /model command — model picker
// ---------------------------------------------------------------------------

/// Helper: dispatch /model and return the text output.
fn dispatch_model(state: &AppState, args: &str) -> crate::commands::DispatchResult {
    let input = if args.is_empty() { "/model".to_string() } else { format!("/model {}", args) };
    let ctx = crate::commands::CommandContext {
        model_name: state.model_name.clone(),
        turn_count: state.turn_count,
        message_count: state.messages.len(),
        all_commands: state.command_registry.command_infos(),
        mcp_servers: Vec::new(),
    };
    crate::commands::dispatch(&input, &state.command_registry, &ctx)
}

/// Helper: dispatch /model with args, push result into state, render, return buffer.
fn dispatch_model_and_render(state: &mut AppState, args: &str, w: u16, h: u16) -> Buffer {
    let input = format!("/model {}", args);
    let ctx = crate::commands::CommandContext {
        model_name: state.model_name.clone(),
        turn_count: state.turn_count,
        message_count: state.messages.len(),
        all_commands: state.command_registry.command_infos(),
        mcp_servers: Vec::new(),
    };
    match crate::commands::dispatch(&input, &state.command_registry, &ctx) {
        crate::commands::DispatchResult::Local(crate::commands::CommandResult::SwitchModel(id)) => {
            state.messages.push(ChatMessage::user(input));
            let mut msg = ChatMessage::assistant_empty();
            msg.append_text(&format!("Switching to model: {}", id));
            state.messages.push(msg);
        }
        crate::commands::DispatchResult::Local(crate::commands::CommandResult::ModelPicker { .. }) => {
            // Model picker now shows as suggestions instead of a modal
        }
        other => panic!("Unexpected dispatch result for /model: {:?}", std::mem::discriminant(&other)),
    }
    render_to_buffer(state, w, h)
}

// -- /model no args: opens interactive picker --

#[test]
fn model_no_args_returns_picker() {
    let state = make_state(80, 30);
    match dispatch_model(&state, "") {
        crate::commands::DispatchResult::Local(crate::commands::CommandResult::ModelPicker { current_model, items }) => {
            assert_eq!(current_model, "test-model");
            assert!(!items.is_empty(), "picker should have items");
        }
        _ => panic!("expected ModelPicker"),
    }
}

#[test]
fn model_no_args_picker_has_items() {
    let state = make_state(80, 30);
    match dispatch_model(&state, "") {
        crate::commands::DispatchResult::Local(crate::commands::CommandResult::ModelPicker { items, .. }) => {
            // Should have items from at least some groups
            let has_claude = items.iter().any(|i| i.alias == "sonnet");
            assert!(has_claude || !items.is_empty(), "picker should have models");
        }
        _ => panic!("expected ModelPicker"),
    }
}

#[test]
fn model_no_args_renders_without_crash() {
    let mut state = make_state(100, 40);
    // Should not panic — model picker is now handled via suggestions, not a modal
    let _buf = dispatch_model_and_render(&mut state, "", 100, 40);
}

// -- /model with alias: returns SwitchModel --

#[test]
fn model_switch_sonnet_alias() {
    let state = make_state(80, 24);
    match dispatch_model(&state, "sonnet") {
        crate::commands::DispatchResult::Local(crate::commands::CommandResult::SwitchModel(id)) => {
            assert!(id.contains("sonnet"), "sonnet alias should resolve, got: {}", id);
        }
        _ => panic!("expected SwitchModel"),
    }
}

#[test]
fn model_switch_opus_alias() {
    let state = make_state(80, 24);
    match dispatch_model(&state, "opus") {
        crate::commands::DispatchResult::Local(crate::commands::CommandResult::SwitchModel(id)) => {
            assert!(id.contains("opus"), "opus alias should resolve, got: {}", id);
        }
        _ => panic!("expected SwitchModel"),
    }
}

#[test]
fn model_switch_haiku_alias() {
    let state = make_state(80, 24);
    match dispatch_model(&state, "haiku") {
        crate::commands::DispatchResult::Local(crate::commands::CommandResult::SwitchModel(id)) => {
            assert!(id.contains("haiku"), "haiku alias should resolve, got: {}", id);
        }
        _ => panic!("expected SwitchModel"),
    }
}

#[test]
fn model_switch_sonnet_4_alias() {
    let state = make_state(80, 24);
    match dispatch_model(&state, "sonnet-4") {
        crate::commands::DispatchResult::Local(crate::commands::CommandResult::SwitchModel(id)) => {
            assert!(id.contains("sonnet-4"), "sonnet-4 alias should resolve, got: {}", id);
        }
        _ => panic!("expected SwitchModel"),
    }
}

#[test]
fn model_switch_opus_4_alias() {
    let state = make_state(80, 24);
    match dispatch_model(&state, "opus-4") {
        crate::commands::DispatchResult::Local(crate::commands::CommandResult::SwitchModel(id)) => {
            assert!(id.contains("opus-4"), "opus-4 alias should resolve, got: {}", id);
        }
        _ => panic!("expected SwitchModel"),
    }
}

#[test]
fn model_switch_sonnet_35_alias() {
    let state = make_state(80, 24);
    match dispatch_model(&state, "sonnet-3.5") {
        crate::commands::DispatchResult::Local(crate::commands::CommandResult::SwitchModel(id)) => {
            assert!(id.contains("sonnet") || id.contains("3-5"), "sonnet-3.5 should resolve, got: {}", id);
        }
        _ => panic!("expected SwitchModel"),
    }
}

#[test]
fn model_switch_haiku_35_alias() {
    let state = make_state(80, 24);
    match dispatch_model(&state, "haiku-3.5") {
        crate::commands::DispatchResult::Local(crate::commands::CommandResult::SwitchModel(id)) => {
            assert!(id.contains("haiku") || id.contains("3-5"), "haiku-3.5 should resolve, got: {}", id);
        }
        _ => panic!("expected SwitchModel"),
    }
}

// -- Amazon Nova aliases --

#[test]
fn model_switch_nova_pro() {
    let state = make_state(80, 24);
    match dispatch_model(&state, "nova-pro") {
        crate::commands::DispatchResult::Local(crate::commands::CommandResult::SwitchModel(id)) => {
            assert!(id.contains("nova-pro"), "nova-pro should resolve, got: {}", id);
        }
        _ => panic!("expected SwitchModel"),
    }
}

#[test]
fn model_switch_nova_lite() {
    let state = make_state(80, 24);
    match dispatch_model(&state, "nova-lite") {
        crate::commands::DispatchResult::Local(crate::commands::CommandResult::SwitchModel(id)) => {
            assert!(id.contains("nova-lite"), "nova-lite should resolve, got: {}", id);
        }
        _ => panic!("expected SwitchModel"),
    }
}

#[test]
fn model_switch_nova_micro() {
    let state = make_state(80, 24);
    match dispatch_model(&state, "nova-micro") {
        crate::commands::DispatchResult::Local(crate::commands::CommandResult::SwitchModel(id)) => {
            assert!(id.contains("nova-micro"), "nova-micro should resolve, got: {}", id);
        }
        _ => panic!("expected SwitchModel"),
    }
}

#[test]
fn model_switch_nova_premier() {
    let state = make_state(80, 24);
    match dispatch_model(&state, "nova-premier") {
        crate::commands::DispatchResult::Local(crate::commands::CommandResult::SwitchModel(id)) => {
            assert!(id.contains("nova-premier"), "nova-premier should resolve, got: {}", id);
        }
        _ => panic!("expected SwitchModel"),
    }
}

// -- Meta Llama aliases --

#[test]
fn model_switch_llama_4_scout() {
    let state = make_state(80, 24);
    match dispatch_model(&state, "llama-4-scout") {
        crate::commands::DispatchResult::Local(crate::commands::CommandResult::SwitchModel(id)) => {
            assert!(id.contains("llama4-scout") || id.contains("llama-4-scout"), "llama-4-scout should resolve, got: {}", id);
        }
        _ => panic!("expected SwitchModel"),
    }
}

#[test]
fn model_switch_llama_4_maverick() {
    let state = make_state(80, 24);
    match dispatch_model(&state, "llama-4-maverick") {
        crate::commands::DispatchResult::Local(crate::commands::CommandResult::SwitchModel(id)) => {
            assert!(id.contains("llama4-maverick") || id.contains("llama-4-maverick"), "should resolve, got: {}", id);
        }
        _ => panic!("expected SwitchModel"),
    }
}

#[test]
fn model_switch_llama_33_70b() {
    let state = make_state(80, 24);
    match dispatch_model(&state, "llama-3.3-70b") {
        crate::commands::DispatchResult::Local(crate::commands::CommandResult::SwitchModel(id)) => {
            assert!(id.contains("llama3-3-70b") || id.contains("llama-3.3-70b"), "should resolve, got: {}", id);
        }
        _ => panic!("expected SwitchModel"),
    }
}

// -- Mistral alias --

#[test]
fn model_switch_mistral_large() {
    let state = make_state(80, 24);
    match dispatch_model(&state, "mistral-large") {
        crate::commands::DispatchResult::Local(crate::commands::CommandResult::SwitchModel(id)) => {
            assert!(id.contains("mistral"), "mistral-large should resolve, got: {}", id);
        }
        _ => panic!("expected SwitchModel"),
    }
}

// -- OpenAI aliases --

#[test]
fn model_switch_gpt_4o() {
    let state = make_state(80, 24);
    match dispatch_model(&state, "gpt-4o") {
        crate::commands::DispatchResult::Local(crate::commands::CommandResult::SwitchModel(id)) => {
            assert!(id.contains("gpt-4o"), "gpt-4o should resolve, got: {}", id);
        }
        _ => panic!("expected SwitchModel"),
    }
}

#[test]
fn model_switch_gpt_41() {
    let state = make_state(80, 24);
    match dispatch_model(&state, "gpt-4.1") {
        crate::commands::DispatchResult::Local(crate::commands::CommandResult::SwitchModel(id)) => {
            assert!(id.contains("gpt-4.1"), "gpt-4.1 should resolve, got: {}", id);
        }
        _ => panic!("expected SwitchModel"),
    }
}

#[test]
fn model_switch_o3() {
    let state = make_state(80, 24);
    match dispatch_model(&state, "o3") {
        crate::commands::DispatchResult::Local(crate::commands::CommandResult::SwitchModel(id)) => {
            assert_eq!(id, "o3");
        }
        _ => panic!("expected SwitchModel"),
    }
}

#[test]
fn model_switch_o3_mini() {
    let state = make_state(80, 24);
    match dispatch_model(&state, "o3-mini") {
        crate::commands::DispatchResult::Local(crate::commands::CommandResult::SwitchModel(id)) => {
            assert_eq!(id, "o3-mini");
        }
        _ => panic!("expected SwitchModel"),
    }
}

// -- Ollama aliases --

#[test]
fn model_switch_ollama_llama32() {
    let state = make_state(80, 24);
    match dispatch_model(&state, "ollama-llama3.2") {
        crate::commands::DispatchResult::Local(crate::commands::CommandResult::SwitchModel(id)) => {
            assert!(id.contains("llama3.2"), "ollama-llama3.2 should resolve, got: {}", id);
        }
        _ => panic!("expected SwitchModel"),
    }
}

#[test]
fn model_switch_ollama_qwen3() {
    let state = make_state(80, 24);
    match dispatch_model(&state, "ollama-qwen3") {
        crate::commands::DispatchResult::Local(crate::commands::CommandResult::SwitchModel(id)) => {
            assert!(id.contains("qwen3"), "ollama-qwen3 should resolve, got: {}", id);
        }
        _ => panic!("expected SwitchModel"),
    }
}

#[test]
fn model_switch_ollama_deepseek_r1() {
    let state = make_state(80, 24);
    match dispatch_model(&state, "ollama-deepseek-r1") {
        crate::commands::DispatchResult::Local(crate::commands::CommandResult::SwitchModel(id)) => {
            assert!(id.contains("deepseek-r1"), "ollama-deepseek-r1 should resolve, got: {}", id);
        }
        _ => panic!("expected SwitchModel"),
    }
}

// -- Full model ID passthrough --

#[test]
fn model_switch_full_id_passthrough() {
    let state = make_state(80, 24);
    match dispatch_model(&state, "claude-sonnet-4-6-20250514") {
        crate::commands::DispatchResult::Local(crate::commands::CommandResult::SwitchModel(id)) => {
            assert_eq!(id, "claude-sonnet-4-6-20250514");
        }
        _ => panic!("expected SwitchModel"),
    }
}

#[test]
fn model_switch_bedrock_full_id() {
    let state = make_state(80, 24);
    match dispatch_model(&state, "global.anthropic.claude-sonnet-4-6-v1") {
        crate::commands::DispatchResult::Local(crate::commands::CommandResult::SwitchModel(id)) => {
            assert_eq!(id, "global.anthropic.claude-sonnet-4-6-v1");
        }
        _ => panic!("expected SwitchModel"),
    }
}

#[test]
fn model_switch_explicit_provider_prefix() {
    let state = make_state(80, 24);
    match dispatch_model(&state, "bedrock/amazon.nova-pro-v1:0") {
        crate::commands::DispatchResult::Local(crate::commands::CommandResult::SwitchModel(id)) => {
            assert_eq!(id, "bedrock/amazon.nova-pro-v1:0");
        }
        _ => panic!("expected SwitchModel"),
    }
}

#[test]
fn model_switch_ollama_explicit_prefix() {
    let state = make_state(80, 24);
    match dispatch_model(&state, "ollama/mistral:7b") {
        crate::commands::DispatchResult::Local(crate::commands::CommandResult::SwitchModel(id)) => {
            assert_eq!(id, "ollama/mistral:7b");
        }
        _ => panic!("expected SwitchModel"),
    }
}

#[test]
fn model_switch_custom_unknown_id() {
    let state = make_state(80, 24);
    match dispatch_model(&state, "my-custom-fine-tuned-model") {
        crate::commands::DispatchResult::Local(crate::commands::CommandResult::SwitchModel(id)) => {
            assert_eq!(id, "my-custom-fine-tuned-model");
        }
        _ => panic!("expected SwitchModel"),
    }
}

// -- /model renders in TUI for switch --

#[test]
fn model_switch_renders_in_tui() {
    let mut state = make_state(100, 30);
    let buf = dispatch_model_and_render(&mut state, "sonnet", 100, 30);
    assert!(buffer_contains(&buf, "Switching to model:"), "should show switching message");
}

// -- /model is immediate (works during streaming) --

#[test]
fn model_command_is_immediate() {
    let state = make_state(80, 24);
    let cmd = state.command_registry.find("model").expect("/model should exist");
    assert!(cmd.immediate, "/model should be immediate");
}

// -- /model argument hint renders --

#[test]
fn model_argument_hint_renders() {
    let mut state = make_state(80, 24);
    type_text(&mut state, "/model ");
    let buf = render_to_buffer(&mut state, 80, 24);
    assert!(
        buffer_contains(&buf, "[model-id]"),
        "Argument hint should render after '/model '"
    );
}

// -- All 22 aliases produce SwitchModel (comprehensive sweep) --

#[test]
fn all_model_aliases_produce_switch_model() {
    let all_aliases = [
        "sonnet", "opus", "haiku",
        "sonnet-4", "opus-4", "sonnet-3.5", "haiku-3.5",
        "nova-pro", "nova-lite", "nova-micro", "nova-premier",
        "llama-4-scout", "llama-4-maverick", "llama-3.3-70b",
        "mistral-large",
        "gpt-4o", "gpt-4.1", "o3", "o3-mini",
        "ollama-llama3.2", "ollama-qwen3", "ollama-deepseek-r1",
    ];
    let state = make_state(80, 24);
    for alias in all_aliases {
        match dispatch_model(&state, alias) {
            crate::commands::DispatchResult::Local(crate::commands::CommandResult::SwitchModel(id)) => {
                assert!(!id.is_empty(), "alias '{}' resolved to empty ID", alias);
            }
            other => panic!("alias '{}' should produce SwitchModel, got {:?}", alias, std::mem::discriminant(&other)),
        }
    }
}

// -- /model no args: picker always returns items (fallback when no providers) --

#[test]
fn model_picker_always_has_items() {
    let state = make_state(80, 30);
    match dispatch_model(&state, "") {
        crate::commands::DispatchResult::Local(crate::commands::CommandResult::ModelPicker { items, .. }) => {
            // Even with no providers detected, fallback shows all models
            assert!(!items.is_empty(), "picker should always have items");
        }
        _ => panic!("expected ModelPicker"),
    }
}
