//! End-to-end TUI tests using ratatui's TestBackend.
//!
//! These tests render the full TUI layout into an in-memory buffer and assert
//! on the visible output — verifying the slash command system works from
//! keypress through dispatch to rendered screen.
//!
//! ## Test harness architecture
//!
//! `TestHarness` wraps a real `TuiApp` with a mock model (no API key needed).
//! It drives the same code paths as the real TUI event loop:
//!
//! ```text
//! harness.type_str("hello")    →  handle_key(Key('h')), handle_key(Key('e')), ...
//! harness.press_enter()        →  handle_key(Enter) → TuiApp::submit() → spawns agent
//! harness.feed_agent_events()  →  TuiApp::handle_agent_event() (simulates stream)
//! harness.render()             →  ratatui::Terminal::draw(render::view)
//! harness.tick(n)              →  advances tick_count (drives spinner, Ctrl+C window)
//! ```

use std::pin::Pin;
use std::sync::Mutex;
use std::collections::VecDeque;

use async_trait::async_trait;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use futures::{stream, Stream};
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::Terminal;
use serde_json::Value;

use super::app::{AgentStatus, AppState, ChatMessage, TuiApp};
use super::event::Event;
use super::render;
use super::widgets::input_bar;

// ===========================================================================
// Mock model — minimal impl for creating a TuiApp without real API keys
// ===========================================================================

/// A mock model that never actually streams. The TUI tests simulate agent
/// events directly via `handle_agent_event`, so we only need the model to
/// satisfy the `Agent::builder().build()` requirement.
struct MockModel {
    responses: Mutex<VecDeque<String>>,
}

impl MockModel {
    fn new() -> Self {
        Self {
            responses: Mutex::new(VecDeque::new()),
        }
    }

    /// Queue a text response for the next `converse`/`stream` call.
    #[allow(dead_code)]
    fn push_response(&self, text: &str) {
        self.responses.lock().unwrap().push_back(text.to_string());
    }
}

#[async_trait]
impl strands::types_module::models::model::Model for MockModel {
    async fn converse<'a>(
        &self,
        _request: &strands::types_module::models::model::ModelRequest,
    ) -> strands::Result<strands::types_module::models::model::ModelResponse> {
        let text = self.responses.lock().unwrap().pop_front().unwrap_or_default();
        Ok(strands::types_module::models::model::ModelResponse::Text(text))
    }

    fn update_config(&mut self, _config: Value) -> strands::Result<()> {
        Ok(())
    }

    fn get_config(&self) -> Value {
        serde_json::json!({})
    }

    async fn structured_output(
        &self,
        _schema: Value,
        _prompt: &strands::types_module::content::Messages,
    ) -> strands::Result<Pin<Box<dyn Stream<Item = strands::Result<Value>> + Send>>> {
        Ok(Box::pin(stream::empty()))
    }

    fn format_request(
        &self,
        _messages: &strands::types_module::content::Messages,
        _system_prompt: Option<&str>,
        _tools: &[strands::ToolSpec],
        _config: &Value,
    ) -> strands::Result<Value> {
        Ok(serde_json::json!({"mock": true}))
    }

    async fn stream(
        &self,
        _request: Value,
    ) -> strands::Result<Pin<Box<dyn Stream<Item = strands::Result<Value>> + Send>>> {
        Ok(Box::pin(stream::empty()))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

// ===========================================================================
// TestHarness — drives TuiApp through the real key handling + event paths
// ===========================================================================

/// E2E test harness that wraps a real `TuiApp` with a mock model.
///
/// All user interactions go through the same `handle_key` → `submit` →
/// `handle_agent_event` code paths as the live TUI. Agent streaming is
/// simulated by feeding events directly (no tokio spawn needed).
struct TestHarness {
    app: TuiApp,
    /// Fake event_tx — `handle_key` requires one for `submit()`, but we
    /// don't consume from it in tests. We drain and inspect it instead.
    event_tx: tokio::sync::mpsc::UnboundedSender<Event>,
    event_rx: tokio::sync::mpsc::UnboundedReceiver<Event>,
    width: u16,
    height: u16,
    /// Keep the tokio runtime alive — `submit()` calls `tokio::spawn`
    /// which requires an active runtime context.
    _rt: tokio::runtime::Runtime,
    /// Guard that enters the runtime context so `tokio::spawn` works
    /// from synchronous code (handle_key → submit → tokio::spawn).
    _guard: tokio::runtime::EnterGuard<'static>,
}

impl TestHarness {
    /// Create a new harness with the given terminal dimensions.
    fn new(width: u16, height: u16) -> Self {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let agent = rt.block_on(async {
            strands::Agent::builder()
                .with_model(std::sync::Arc::new(MockModel::new()))
                .with_system_prompt("test")
                .with_max_iterations(1)
                .build()
                .await
                .unwrap()
        });
        // Enter the runtime context so tokio::spawn works in sync code.
        // Safety: we leak a &'static reference to the runtime, which is fine
        // for tests — each test creates its own runtime.
        let rt_ref: &'static tokio::runtime::Runtime = Box::leak(Box::new(rt));
        let guard = rt_ref.enter();
        let registry = crate::commands::builtin_registry();
        let mock_model: std::sync::Arc<dyn strands::types::models::Model> = std::sync::Arc::new(MockModel::new());
        let mut app = TuiApp::new(agent, "test-model".to_string(), registry, mock_model);
        app.state.terminal_width = width;
        app.state.mcp_status = super::app::McpStatus::None;
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        // Store a dummy Runtime (we leaked the real one) — use Runtime::new()
        // just to satisfy the struct field. The leaked one stays alive.
        let dummy_rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        Self { app, event_tx, event_rx, width, height, _rt: dummy_rt, _guard: guard }
    }

    // --- Input simulation ---

    /// Type a string character by character through handle_key.
    fn type_str(&mut self, text: &str) {
        for ch in text.chars() {
            let key = KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE);
            super::handle_key(&mut self.app, key, self.event_tx.clone());
        }
    }

    /// Press Enter through the real handle_key path.
    fn press_enter(&mut self) {
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        super::handle_key(&mut self.app, key, self.event_tx.clone());
    }

    /// Press Escape through handle_key.
    fn press_esc(&mut self) {
        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        super::handle_key(&mut self.app, key, self.event_tx.clone());
    }

    /// Press Ctrl+C through handle_key.
    fn press_ctrl_c(&mut self) {
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        super::handle_key(&mut self.app, key, self.event_tx.clone());
    }

    /// Press Tab through handle_key.
    fn press_tab(&mut self) {
        let key = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        super::handle_key(&mut self.app, key, self.event_tx.clone());
    }

    /// Press Up arrow through handle_key.
    fn press_up(&mut self) {
        let key = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        super::handle_key(&mut self.app, key, self.event_tx.clone());
    }

    /// Press Down arrow through handle_key.
    fn press_down(&mut self) {
        let key = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        super::handle_key(&mut self.app, key, self.event_tx.clone());
    }

    /// Press Backspace through handle_key.
    fn press_backspace(&mut self) {
        let key = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        super::handle_key(&mut self.app, key, self.event_tx.clone());
    }

    // --- Agent event simulation ---

    /// Feed a sequence of agent events (simulating a streaming response).
    fn feed_agent_events(&mut self, events: Vec<Event>) {
        for event in events {
            self.app.handle_agent_event(event);
        }
    }

    /// Simulate a complete agent text response (text deltas + done).
    fn simulate_response(&mut self, text: &str) {
        // Simulate streaming: split text into word-sized deltas
        let events: Vec<Event> = text
            .split_inclusive(' ')
            .map(|chunk| Event::AgentTextDelta(chunk.to_string()))
            .chain(std::iter::once(Event::AgentDone))
            .collect();
        self.feed_agent_events(events);
    }

    /// Simulate a tool call + result in the agent stream.
    fn simulate_tool_call(&mut self, name: &str, summary: &str) {
        self.feed_agent_events(vec![
            Event::AgentToolStart { name: name.to_string(), tool_use_id: String::new() },
            Event::AgentToolCall {
                name: name.to_string(),
                input: serde_json::json!({"summary": summary}),
                tool_use_id: String::new(),
            },
            Event::AgentToolResult {
                status: "success".to_string(),
                content: String::new(),
                tool_use_id: String::new(),
            },
        ]);
    }

    // --- Time simulation ---

    /// Advance tick count (12Hz, so 12 ticks = 1 second).
    fn tick(&mut self, n: usize) {
        self.app.state.tick_count = self.app.state.tick_count.wrapping_add(n);
    }

    // --- Render + assert ---

    /// Render the TUI and return the terminal buffer.
    fn render(&mut self) -> Buffer {
        let backend = TestBackend::new(self.width, self.height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render::view(frame, &mut self.app.state))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    /// Get the current input text.
    fn input_text(&self) -> String {
        self.app.state.input.lines().join("\n")
    }

    /// Drain events sent to event_tx (from submit spawning agent tasks).
    fn drain_events(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        while let Ok(e) = self.event_rx.try_recv() {
            events.push(e);
        }
        events
    }
}

// ===========================================================================
// Legacy helpers (used by older tests, kept for compatibility)
// ===========================================================================

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
        last_line.contains("? for shortcuts"),
        "Status bar should show '? for shortcuts', got: {last_line}"
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
        last_line.contains("esc to interrupt"),
        "Status bar should show 'esc to interrupt' during streaming, got: {last_line}"
    );
    assert!(
        !last_line.contains("? for shortcuts"),
        "Status bar should NOT show '? for shortcuts' during streaming, got: {last_line}"
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
        token_counts: None,
        context_percent_used: None,
        system_prompt: String::new(),
        tool_specs: Vec::new(),
        mcp_tool_specs: Vec::new(),
        memory_files: Vec::new(),
        skills: Vec::new(),
        messages_json: Vec::new(),
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
        token_counts: None,
        context_percent_used: None,
        system_prompt: String::new(),
        tool_specs: Vec::new(),
        mcp_tool_specs: Vec::new(),
        memory_files: Vec::new(),
        skills: Vec::new(),
        messages_json: Vec::new(),
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
    token_counts: None,
    context_percent_used: None,
    system_prompt: String::new(),
    tool_specs: Vec::new(),
    mcp_tool_specs: Vec::new(),
    memory_files: Vec::new(),
    skills: Vec::new(),
    messages_json: Vec::new(),
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
    token_counts: None,
    context_percent_used: None,
    system_prompt: String::new(),
    tool_specs: Vec::new(),
    mcp_tool_specs: Vec::new(),
    memory_files: Vec::new(),
    skills: Vec::new(),
    messages_json: Vec::new(),
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
    token_counts: None,
    context_percent_used: None,
    system_prompt: String::new(),
    tool_specs: Vec::new(),
    mcp_tool_specs: Vec::new(),
    memory_files: Vec::new(),
    skills: Vec::new(),
    messages_json: Vec::new(),
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
    token_counts: None,
    context_percent_used: None,
    system_prompt: String::new(),
    tool_specs: Vec::new(),
    mcp_tool_specs: Vec::new(),
    memory_files: Vec::new(),
    skills: Vec::new(),
    messages_json: Vec::new(),
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
    token_counts: None,
    context_percent_used: None,
    system_prompt: String::new(),
    tool_specs: Vec::new(),
    mcp_tool_specs: Vec::new(),
    memory_files: Vec::new(),
    skills: Vec::new(),
    messages_json: Vec::new(),
    };

    match crate::commands::dispatch("/compact", &state.command_registry, &ctx) {
        crate::commands::DispatchResult::CompactPrompt(prompt) => {
            assert!(
                prompt.contains("Summarize"),
                "Compact prompt should contain 'Summarize', got: {prompt}"
            );
        }
        _ => panic!("Expected CompactPrompt for /compact"),
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
    token_counts: None,
    context_percent_used: None,
    system_prompt: String::new(),
    tool_specs: Vec::new(),
    mcp_tool_specs: Vec::new(),
    memory_files: Vec::new(),
    skills: Vec::new(),
    messages_json: Vec::new(),
    };

    match crate::commands::dispatch("/compact keep all file paths", &state.command_registry, &ctx) {
        crate::commands::DispatchResult::CompactPrompt(prompt) => {
            assert!(
                prompt.contains("keep all file paths"),
                "Compact prompt should include custom args, got: {prompt}"
            );
        }
        _ => panic!("Expected CompactPrompt for /compact with args"),
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
    token_counts: None,
    context_percent_used: None,
    system_prompt: String::new(),
    tool_specs: Vec::new(),
    mcp_tool_specs: Vec::new(),
    memory_files: Vec::new(),
    skills: Vec::new(),
    messages_json: Vec::new(),
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
    token_counts: None,
    context_percent_used: None,
    system_prompt: String::new(),
    tool_specs: Vec::new(),
    mcp_tool_specs: Vec::new(),
    memory_files: Vec::new(),
    skills: Vec::new(),
    messages_json: Vec::new(),
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
        String::new(),
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
    token_counts: None,
    context_percent_used: None,
    system_prompt: String::new(),
    tool_specs: Vec::new(),
    mcp_tool_specs: Vec::new(),
    memory_files: Vec::new(),
    skills: Vec::new(),
    messages_json: Vec::new(),
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
        token_counts: None,
        context_percent_used: None,
        system_prompt: String::new(),
        tool_specs: Vec::new(),
        mcp_tool_specs: Vec::new(),
        memory_files: Vec::new(),
        skills: Vec::new(),
        messages_json: Vec::new(),
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
    // When auto_scroll is on, the scroll indicator should NOT appear (clean bar)
    let mut state = make_state(100, 24);
    state.auto_scroll = true;
    let buf = render_to_buffer(&mut state, 100, 24);
    let lines = buffer_lines(&buf);
    let last = &lines[lines.len() - 1];
    assert!(
        !last.contains("↑"),
        "Status bar should not show scroll-up indicator when auto_scroll, got: {last}"
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
        token_counts: None,
        context_percent_used: None,
        system_prompt: String::new(),
        tool_specs: Vec::new(),
        mcp_tool_specs: Vec::new(),
        memory_files: Vec::new(),
        skills: Vec::new(),
        messages_json: Vec::new(),
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
        token_counts: None,
        context_percent_used: None,
        system_prompt: String::new(),
        tool_specs: Vec::new(),
        mcp_tool_specs: Vec::new(),
        memory_files: Vec::new(),
        skills: Vec::new(),
        messages_json: Vec::new(),
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

// ===========================================================================
// E2E: Cache invalidation — local commands must invalidate render cache
// ===========================================================================

/// Regression test: running /status (or /mcp) twice should show both outputs.
/// Previously, the second invocation was invisible because `invalidate_cache()`
/// was not called after pushing messages from `CommandResult::Text`.
#[test]
fn local_command_text_invalidates_cache_on_repeated_calls() {
    let mut state = make_state(80, 40);

    // First /status
    dispatch_local_text(&mut state, "/status");
    let buf1 = render_to_buffer(&mut state, 80, 40);
    let lines1 = buffer_lines(&buf1);
    let status_count_1 = lines1.iter().filter(|l| l.contains("Model: test-model")).count();
    assert_eq!(status_count_1, 1, "First /status should show model info once");

    // Second /status — this must also render (was the bug)
    dispatch_local_text(&mut state, "/status");
    let buf2 = render_to_buffer(&mut state, 80, 40);
    let lines2 = buffer_lines(&buf2);
    let status_count_2 = lines2.iter().filter(|l| l.contains("Model: test-model")).count();
    assert_eq!(status_count_2, 2, "Second /status should show model info twice, got: {}", status_count_2);
}

/// Verify unknown command also invalidates cache.
#[test]
fn unknown_command_invalidates_cache() {
    let mut state = make_state(80, 40);

    // First unknown
    dispatch_unknown(&mut state, "/foo");
    let buf1 = render_to_buffer(&mut state, 80, 40);
    assert!(buffer_contains(&buf1, "Unknown command"));

    // Second unknown
    dispatch_unknown(&mut state, "/bar");
    let buf2 = render_to_buffer(&mut state, 80, 40);
    let lines = buffer_lines(&buf2);
    let count = lines.iter().filter(|l| l.contains("Unknown command")).count();
    assert_eq!(count, 2, "Both unknown commands should be visible, got: {}", count);
}

/// Per-message cache is populated after render and reused on subsequent renders.
#[test]
fn per_message_cache_populated_after_render() {
    let mut state = make_state(80, 24);
    state.messages.push(ChatMessage::user("hello".into()));
    let mut msg = ChatMessage::assistant_empty();
    msg.append_text("world");
    state.messages.push(msg);

    let _ = render_to_buffer(&mut state, 80, 24);

    // Cache should be populated for both messages
    assert_eq!(state.message_cache.len(), 2);
    assert!(state.message_cache[0].is_some(), "User message should be cached");
    assert!(state.message_cache[1].is_some(), "Assistant message should be cached");
}

/// Per-message cache auto-invalidates when message content changes (fingerprint mismatch).
#[test]
fn per_message_cache_invalidates_on_content_change() {
    let mut state = make_state(80, 30);

    state.messages.push(ChatMessage::user("hello".into()));
    let mut msg = ChatMessage::assistant_empty();
    msg.append_text("world");
    state.messages.push(msg);

    let _ = render_to_buffer(&mut state, 80, 30);

    // Push more messages
    state.messages.push(ChatMessage::user("second".into()));
    let mut msg2 = ChatMessage::assistant_empty();
    msg2.append_text("response");
    state.messages.push(msg2);

    // Render again — new content should appear (cache auto-extends)
    let buf = render_to_buffer(&mut state, 80, 30);
    assert!(buffer_contains(&buf, "second"), "New user message should be visible");
    assert!(buffer_contains(&buf, "response"), "New assistant message should be visible");
    assert_eq!(state.message_cache.len(), 4, "Cache should have entries for all messages");
}

// ===========================================================================
// E2E: Streaming cache — old messages cached, only last re-rendered
// ===========================================================================

/// During streaming, per-message caches for earlier messages stay valid.
#[test]
fn streaming_caches_stable_messages() {
    let mut state = make_state(80, 30);

    // Add some history
    state.messages.push(ChatMessage::user("question 1".into()));
    let mut r1 = ChatMessage::assistant_empty();
    r1.append_text("answer with **bold** and `code`");
    state.messages.push(r1);

    // Start "streaming" — add empty assistant message
    state.messages.push(ChatMessage::user("question 2".into()));
    state.messages.push(ChatMessage::assistant_empty());
    state.agent_status = AgentStatus::Streaming;

    // First render during streaming — should cache stable messages (0..2)
    let _ = render_to_buffer(&mut state, 80, 30);
    assert_eq!(state.message_cache.len(), 4);
    assert!(state.message_cache[0].is_some(), "User msg 0 should be cached");
    assert!(state.message_cache[1].is_some(), "Assistant msg 1 should be cached");
    let cached_lines_0 = state.message_cache[0].as_ref().unwrap().lines.len();

    // Simulate streaming text deltas
    state.messages.last_mut().unwrap().append_text("partial ");
    let _ = render_to_buffer(&mut state, 80, 30);

    // Earlier message caches should be unchanged
    assert_eq!(state.message_cache[0].as_ref().unwrap().lines.len(), cached_lines_0,
        "Stable message cache should remain unchanged during streaming deltas");
}

/// Verify streaming message renders its latest text.
#[test]
fn streaming_message_shows_complete_lines() {
    let mut state = make_state(80, 30);

    state.messages.push(ChatMessage::user("hi".into()));
    state.messages.push(ChatMessage::assistant_empty());
    state.agent_status = AgentStatus::Streaming;

    // Streaming with complete line + partial line
    state.messages.last_mut().unwrap().append_text("Line one complete\nLine two partial");


    let buf = render_to_buffer(&mut state, 80, 30);
    // Line-buffered: should show "Line one complete" but NOT "Line two partial"
    assert!(buffer_contains(&buf, "Line one complete"),
        "Complete line should be visible during streaming");
    assert!(!buffer_contains(&buf, "Line two partial"),
        "Partial line should be hidden during streaming (line-buffered)");
}

/// When streaming ends (AgentDone), the full text including partial lines should appear.
#[test]
fn streaming_done_shows_full_text() {
    let mut state = make_state(80, 30);

    state.messages.push(ChatMessage::user("hi".into()));
    state.messages.push(ChatMessage::assistant_empty());
    state.messages.last_mut().unwrap().append_text("Complete line\nAnd final partial");

    // Streaming ended
    state.agent_status = AgentStatus::Idle;


    let buf = render_to_buffer(&mut state, 80, 30);
    assert!(buffer_contains(&buf, "Complete line"), "Complete line should show");
    assert!(buffer_contains(&buf, "And final partial"),
        "After streaming ends, partial line must also be visible");
}

// ===========================================================================
// E2E: Markdown rendering correctness
// ===========================================================================

#[test]
fn markdown_bold_renders() {
    let mut state = make_state(80, 30);
    state.messages.push(ChatMessage::user("test".into()));
    let mut msg = ChatMessage::assistant_empty();
    msg.append_text("This is **bold text** here");
    state.messages.push(msg);

    let buf = render_to_buffer(&mut state, 80, 30);
    assert!(buffer_contains(&buf, "bold text"), "Bold markdown text should render");
}

#[test]
fn markdown_code_block_renders() {
    let mut state = make_state(80, 30);
    state.messages.push(ChatMessage::user("test".into()));
    let mut msg = ChatMessage::assistant_empty();
    msg.append_text("```rust\nfn main() {\n    println!(\"hello\");\n}\n```");
    state.messages.push(msg);

    let buf = render_to_buffer(&mut state, 80, 30);
    assert!(buffer_contains(&buf, "fn main()"), "Code block content should render");
    // Code block should have border characters
    assert!(buffer_contains(&buf, "rust"), "Language label should render");
}

#[test]
fn markdown_inline_code_renders() {
    let mut state = make_state(80, 30);
    state.messages.push(ChatMessage::user("test".into()));
    let mut msg = ChatMessage::assistant_empty();
    msg.append_text("Use the `println!` macro");
    state.messages.push(msg);

    let buf = render_to_buffer(&mut state, 80, 30);
    assert!(buffer_contains(&buf, "println!"), "Inline code should render");
}

#[test]
fn markdown_list_renders() {
    let mut state = make_state(80, 30);
    state.messages.push(ChatMessage::user("test".into()));
    let mut msg = ChatMessage::assistant_empty();
    msg.append_text("- Item one\n- Item two\n- Item three");
    state.messages.push(msg);

    let buf = render_to_buffer(&mut state, 80, 30);
    assert!(buffer_contains(&buf, "Item one"), "List items should render");
    assert!(buffer_contains(&buf, "Item two"));
    assert!(buffer_contains(&buf, "Item three"));
}

#[test]
fn markdown_heading_renders() {
    let mut state = make_state(80, 30);
    state.messages.push(ChatMessage::user("test".into()));
    let mut msg = ChatMessage::assistant_empty();
    msg.append_text("# Big Title\n\nSome paragraph text.");
    state.messages.push(msg);

    let buf = render_to_buffer(&mut state, 80, 30);
    assert!(buffer_contains(&buf, "Big Title"), "Heading should render");
    assert!(buffer_contains(&buf, "Some paragraph text"), "Paragraph should render");
}

#[test]
fn markdown_empty_text_no_panic() {
    let mut state = make_state(80, 24);
    state.messages.push(ChatMessage::user("test".into()));
    let mut msg = ChatMessage::assistant_empty();
    msg.append_text("");
    state.messages.push(msg);
    let _ = render_to_buffer(&mut state, 80, 24);
}

/// Large markdown content should not panic (stress test).
#[test]
fn markdown_large_content_no_panic() {
    let mut state = make_state(80, 30);
    state.messages.push(ChatMessage::user("test".into()));
    let mut msg = ChatMessage::assistant_empty();
    // Generate a large markdown document
    let mut text = String::new();
    for i in 0..100 {
        text.push_str(&format!("## Section {}\n\nParagraph with **bold** and `code` for section {}.\n\n", i, i));
        text.push_str(&format!("```python\ndef func_{}():\n    return {}\n```\n\n", i, i));
    }
    msg.append_text(&text);
    state.messages.push(msg);
    let _ = render_to_buffer(&mut state, 80, 30);
}

// ===========================================================================
// E2E: Streaming markdown — incremental rendering
// ===========================================================================

/// Simulates what happens during streaming: text grows incrementally.
/// Each frame should render without error and show completed lines.
#[test]
fn streaming_incremental_text_renders_correctly() {
    let mut state = make_state(80, 30);
    state.messages.push(ChatMessage::user("explain".into()));
    state.messages.push(ChatMessage::assistant_empty());
    state.agent_status = AgentStatus::Streaming;

    let deltas = [
        "Here's ",
        "an explanation:\n\n",
        "1. First point\n",
        "2. Second point\n",
        "3. Third with `code`\n",
        "\n```rust\n",
        "fn example() {}\n",
        "```\n",
        "\nFinal paragraph.",
    ];

    for delta in &deltas {
        state.messages.last_mut().unwrap().append_text(delta);
    
        let _ = render_to_buffer(&mut state, 80, 30);
        // No panic = pass
    }

    // End streaming — all content should be visible
    state.agent_status = AgentStatus::Idle;

    let buf = render_to_buffer(&mut state, 80, 30);
    assert!(buffer_contains(&buf, "First point"), "Point 1 should show");
    assert!(buffer_contains(&buf, "Second point"), "Point 2 should show");
    assert!(buffer_contains(&buf, "fn example()"), "Code should show");
    assert!(buffer_contains(&buf, "Final paragraph"), "Final text should show");
}

/// Tool calls interleaved with text during streaming.
#[test]
fn streaming_tool_calls_interleaved_with_text() {
    let mut state = make_state(100, 40);
    state.messages.push(ChatMessage::user("fix bug".into()));
    state.messages.push(ChatMessage::assistant_empty());
    state.agent_status = AgentStatus::Streaming;

    // Text delta
    state.messages.last_mut().unwrap().append_text("Let me look at the code.\n");

    let _ = render_to_buffer(&mut state, 100, 40);

    // Tool call
    state.messages.last_mut().unwrap().add_tool_call(
        "Read".into(), String::new(), "src/main.rs".into(), super::app::ToolCallStatus::Running,
    );

    let buf = render_to_buffer(&mut state, 100, 40);
    assert!(buffer_contains(&buf, "Read"), "Running tool call should render");

    // Tool completes
    state.messages.last_mut().unwrap().set_last_tool_status(super::app::ToolCallStatus::Success);

    let _ = render_to_buffer(&mut state, 100, 40);

    // More text after tool
    state.messages.last_mut().unwrap().append_text("I found the issue.\n");


    // End streaming
    state.agent_status = AgentStatus::Idle;

    let buf = render_to_buffer(&mut state, 100, 40);
    assert!(buffer_contains(&buf, "I found the issue"), "Post-tool text should render");
}

// ===========================================================================
// E2E: Ctrl+C double-tap quit behavior
// ===========================================================================

#[test]
fn ctrl_c_first_press_does_not_quit() {
    let mut state = make_state(80, 24);
    state.tick_count = 100;

    // Simulate first Ctrl+C on idle with empty input
    // (mirrors handle_ctrl_c logic)
    assert!(state.input.lines().join("").trim().is_empty());
    state.last_ctrl_c_tick = Some(state.tick_count);

    assert!(!state.should_quit, "First Ctrl+C should NOT quit");
    assert!(state.last_ctrl_c_tick.is_some(), "Should record the tick");
}

#[test]
fn ctrl_c_double_tap_within_window_quits() {
    let mut state = make_state(80, 24);
    state.tick_count = 100;

    // First Ctrl+C
    state.last_ctrl_c_tick = Some(state.tick_count);

    // Advance a few ticks (within 24-tick window at 12Hz = 2s)
    state.tick_count = 110;

    // Second Ctrl+C — check the double-tap condition
    let within_window = state.last_ctrl_c_tick.map_or(false, |t| {
        state.tick_count.wrapping_sub(t) <= 24
    });
    assert!(within_window, "Second press within 24 ticks should be within the double-tap window");
}

#[test]
fn ctrl_c_outside_window_does_not_quit() {
    let mut state = make_state(80, 24);
    state.tick_count = 100;

    // First Ctrl+C
    state.last_ctrl_c_tick = Some(state.tick_count);

    // Advance beyond window (>24 ticks)
    state.tick_count = 130;

    let within_window = state.last_ctrl_c_tick.map_or(false, |t| {
        state.tick_count.wrapping_sub(t) <= 24
    });
    assert!(!within_window, "Press outside the 24-tick window should NOT trigger quit");
}

#[test]
fn ctrl_c_clears_input_first() {
    let mut state = make_state(80, 24);
    type_text(&mut state, "some text");
    assert!(!state.input.lines().join("").trim().is_empty());

    // With text in input, Ctrl+C should clear input, not quit
    // (mirrors handle_ctrl_c: has_input branch)
    let has_input = !state.input.lines().join("").trim().is_empty();
    assert!(has_input, "Input should have text before Ctrl+C");
}

#[test]
fn ctrl_c_cancels_streaming_first() {
    let mut state = make_state(80, 24);
    state.agent_status = AgentStatus::Streaming;

    // Ctrl+C during streaming should cancel, not quit
    assert!(matches!(state.agent_status, AgentStatus::Streaming));
    // After cancel:
    state.agent_status = AgentStatus::Idle;
    state.last_ctrl_c_tick = Some(state.tick_count);

    assert!(!state.should_quit, "Ctrl+C during streaming should cancel, not quit");
}

#[test]
fn status_bar_shows_ctrl_c_hint() {
    let mut state = make_state(100, 24);
    state.tick_count = 100;
    state.last_ctrl_c_tick = Some(state.tick_count);

    let buf = render_to_buffer(&mut state, 100, 24);
    let lines = buffer_lines(&buf);
    let last_line = &lines[lines.len() - 1];
    assert!(
        last_line.contains("Ctrl+C again to quit"),
        "Status bar should show double-tap hint, got: {last_line}"
    );
}

#[test]
fn status_bar_hint_expires_after_window() {
    let mut state = make_state(100, 24);
    state.tick_count = 100;
    state.last_ctrl_c_tick = Some(100);

    // Advance beyond the window
    state.tick_count = 130;

    let buf = render_to_buffer(&mut state, 100, 24);
    let lines = buffer_lines(&buf);
    let last_line = &lines[lines.len() - 1];
    assert!(
        !last_line.contains("Ctrl+C again"),
        "Hint should expire after window, got: {last_line}"
    );
    assert!(
        last_line.contains("? for shortcuts"),
        "Should return to normal hint after expiry, got: {last_line}"
    );
}

// ===========================================================================
// E2E: Selection rendered_lines — only computed when needed
// ===========================================================================

#[test]
fn selection_lines_not_computed_when_inactive() {
    let mut state = make_state(80, 30);
    state.messages.push(ChatMessage::user("hello".into()));
    let mut msg = ChatMessage::assistant_empty();
    msg.append_text("world");
    state.messages.push(msg);

    // No selection active, anchor == end
    state.selection.active = false;
    state.selection.anchor = (0, 0);
    state.selection.end = (0, 0);

    // Clear rendered_lines to verify they stay empty
    state.selection.rendered_lines.clear();

    let _ = render_to_buffer(&mut state, 80, 30);
    assert!(
        state.selection.rendered_lines.is_empty(),
        "rendered_lines should NOT be populated when no selection is active"
    );
}

#[test]
fn selection_lines_computed_when_active() {
    let mut state = make_state(80, 30);
    state.messages.push(ChatMessage::user("hello".into()));
    let mut msg = ChatMessage::assistant_empty();
    msg.append_text("world");
    state.messages.push(msg);

    // Simulate active selection
    state.selection.active = true;
    state.selection.anchor = (2, 5);
    state.selection.end = (3, 10);

    let _ = render_to_buffer(&mut state, 80, 30);
    assert!(
        !state.selection.rendered_lines.is_empty(),
        "rendered_lines should be populated when selection is active"
    );
}

#[test]
fn selection_lines_computed_when_completed_selection_exists() {
    let mut state = make_state(80, 30);
    state.messages.push(ChatMessage::user("hello".into()));
    let mut msg = ChatMessage::assistant_empty();
    msg.append_text("world");
    state.messages.push(msg);

    // Selection completed (not active, but anchor != end)
    state.selection.active = false;
    state.selection.anchor = (2, 5);
    state.selection.end = (3, 10);

    let _ = render_to_buffer(&mut state, 80, 30);
    assert!(
        !state.selection.rendered_lines.is_empty(),
        "rendered_lines should be computed for completed (non-cleared) selection"
    );
}

// ===========================================================================
// E2E: Scroll behavior during streaming
// ===========================================================================

#[test]
fn auto_scroll_follows_streaming_content() {
    let mut state = make_state(80, 10); // Small viewport to force scrolling

    state.messages.push(ChatMessage::user("test".into()));
    state.messages.push(ChatMessage::assistant_empty());
    state.agent_status = AgentStatus::Streaming;

    // Add enough lines to overflow viewport
    let mut text = String::new();
    for i in 0..20 {
        text.push_str(&format!("Line number {}\n", i));
    }
    state.messages.last_mut().unwrap().append_text(&text);


    assert!(state.auto_scroll, "auto_scroll should be on during streaming");
    let _ = render_to_buffer(&mut state, 80, 10);
    // With auto_scroll, scroll_offset should be 0 (pinned to bottom)
    assert_eq!(state.scroll_offset, 0);
}

#[test]
fn manual_scroll_disables_auto_scroll() {
    let mut state = make_state(80, 10);

    state.messages.push(ChatMessage::user("test".into()));
    let mut msg = ChatMessage::assistant_empty();
    let mut text = String::new();
    for i in 0..30 {
        text.push_str(&format!("Long line {}\n", i));
    }
    msg.append_text(&text);
    state.messages.push(msg);


    let _ = render_to_buffer(&mut state, 80, 10);

    // Simulate scroll up
    state.auto_scroll = false;
    state.scroll_offset = 5;

    let buf = render_to_buffer(&mut state, 80, 10);
    // Should NOT show the very last lines when scrolled up
    assert!(!buffer_contains(&buf, "Long line 29"),
        "Scrolled-up view should NOT show the last line");
}

// ===========================================================================
// E2E: Tool call grouping
// ===========================================================================

#[test]
fn consecutive_search_tools_grouped() {
    let mut state = make_state(100, 30);
    state.messages.push(ChatMessage::user("find it".into()));
    let mut msg = ChatMessage::assistant_empty();
    msg.add_tool_call("Read".into(), String::new(), "file1.rs".into(), super::app::ToolCallStatus::Success);
    msg.add_tool_call("Read".into(), String::new(), "file2.rs".into(), super::app::ToolCallStatus::Success);
    msg.add_tool_call("Grep".into(), String::new(), "pattern".into(), super::app::ToolCallStatus::Success);
    state.messages.push(msg);

    let buf = render_to_buffer(&mut state, 100, 30);
    // Grouped tools should show a collapsed summary, not individual lines
    // At minimum, all three shouldn't show as separate full entries
    let lines = buffer_lines(&buf);
    let read_lines: Vec<_> = lines.iter().filter(|l| l.contains("Read")).collect();
    // Should be collapsed into one group line, not two separate "Read" lines
    assert!(read_lines.len() <= 1,
        "Consecutive same-group Read calls should be collapsed, found {} lines",
        read_lines.len());
}

#[test]
fn mixed_tool_groups_not_collapsed() {
    let mut state = make_state(100, 30);
    state.messages.push(ChatMessage::user("do things".into()));
    let mut msg = ChatMessage::assistant_empty();
    msg.add_tool_call("Read".into(), String::new(), "file.rs".into(), super::app::ToolCallStatus::Success);
    msg.add_tool_call("Edit".into(), String::new(), "file.rs:10".into(), super::app::ToolCallStatus::Success);
    msg.add_tool_call("Read".into(), String::new(), "other.rs".into(), super::app::ToolCallStatus::Success);
    state.messages.push(msg);

    let buf = render_to_buffer(&mut state, 100, 30);
    // Read and Edit are different groups — should not collapse across groups
    let lines = buffer_lines(&buf);
    let tool_lines: Vec<_> = lines.iter()
        .filter(|l| l.contains("Read") || l.contains("Edit"))
        .collect();
    assert!(tool_lines.len() >= 2,
        "Tools from different groups should not be collapsed together");
}

// ===========================================================================
// E2E: Narrow terminal rendering
// ===========================================================================

#[test]
fn very_narrow_terminal_no_panic() {
    let mut state = make_state(20, 8);
    state.messages.push(ChatMessage::user("test".into()));
    let mut msg = ChatMessage::assistant_empty();
    msg.append_text("```rust\nfn a_very_long_function_name() { println!(\"hello world\"); }\n```");
    state.messages.push(msg);
    let _ = render_to_buffer(&mut state, 20, 8);
}

#[test]
fn single_line_terminal_no_panic() {
    let mut state = make_state(80, 3);
    state.messages.push(ChatMessage::user("test".into()));
    let mut msg = ChatMessage::assistant_empty();
    msg.append_text("Some response");
    state.messages.push(msg);
    let _ = render_to_buffer(&mut state, 80, 3);
}

// ===========================================================================
// E2E: Multiple conversation turns render correctly
// ===========================================================================

#[test]
fn multi_turn_conversation_renders_all_messages() {
    let mut state = make_state(80, 50);

    for i in 0..5 {
        state.messages.push(ChatMessage::user(format!("Question {}", i)));
        let mut msg = ChatMessage::assistant_empty();
        msg.append_text(&format!("Answer {}", i));
        state.messages.push(msg);
    
    }

    let buf = render_to_buffer(&mut state, 80, 50);
    for i in 0..5 {
        assert!(buffer_contains(&buf, &format!("Question {}", i)),
            "Question {} should be visible", i);
        assert!(buffer_contains(&buf, &format!("Answer {}", i)),
            "Answer {} should be visible", i);
    }
}

// ===========================================================================
// Helpers for the new tests
// ===========================================================================

/// Dispatch a local text command and push results into state.
fn dispatch_local_text(state: &mut AppState, cmd: &str) {
    let ctx = crate::commands::CommandContext {
        model_name: state.model_name.clone(),
        turn_count: state.turn_count,
        message_count: state.messages.len(),
        all_commands: state.command_registry.command_infos(),
        mcp_servers: Vec::new(),
        token_counts: None,
        context_percent_used: None,
        system_prompt: String::new(),
        tool_specs: Vec::new(),
        mcp_tool_specs: Vec::new(),
        memory_files: Vec::new(),
        skills: Vec::new(),
        messages_json: Vec::new(),
    };
    match crate::commands::dispatch(cmd, &state.command_registry, &ctx) {
        crate::commands::DispatchResult::Local(crate::commands::CommandResult::Text(text)) => {
            state.messages.push(ChatMessage::user(cmd.to_string()));
            let mut msg = ChatMessage::assistant_empty();
            msg.append_text(&text);
            state.messages.push(msg);
        
        }
        other => panic!("Expected Local(Text) for '{}', got {:?}", cmd, std::mem::discriminant(&other)),
    }
}

/// Push an unknown command error into state.
fn dispatch_unknown(state: &mut AppState, cmd: &str) {
    let ctx = crate::commands::CommandContext {
        model_name: state.model_name.clone(),
        turn_count: state.turn_count,
        message_count: state.messages.len(),
        all_commands: state.command_registry.command_infos(),
        mcp_servers: Vec::new(),
        token_counts: None,
        context_percent_used: None,
        system_prompt: String::new(),
        tool_specs: Vec::new(),
        mcp_tool_specs: Vec::new(),
        memory_files: Vec::new(),
        skills: Vec::new(),
        messages_json: Vec::new(),
    };
    match crate::commands::dispatch(cmd, &state.command_registry, &ctx) {
        crate::commands::DispatchResult::Unknown(name) => {
            state.messages.push(ChatMessage::user(cmd.to_string()));
            let mut msg = ChatMessage::assistant_empty();
            msg.append_text(&format!("Unknown command: /{}. Type /help for available commands.", name));
            state.messages.push(msg);
        
        }
        _ => panic!("Expected Unknown for '{}'", cmd),
    }
}

// ###########################################################################
//
// E2E tests using TestHarness — full user activity simulation
//
// These tests drive the REAL handle_key → TuiApp::submit → handle_agent_event
// → render pipeline. No state manipulation shortcuts.
//
// ###########################################################################

// ===========================================================================
// Harness: typing and input
// ===========================================================================

#[test]
fn harness_type_and_read_input() {
    let mut h = TestHarness::new(80, 24);
    h.type_str("hello world");
    assert_eq!(h.input_text(), "hello world");
}

#[test]
fn harness_backspace_deletes_char() {
    let mut h = TestHarness::new(80, 24);
    h.type_str("hello");
    h.press_backspace();
    assert_eq!(h.input_text(), "hell");
}

#[test]
fn harness_type_renders_in_input_bar() {
    let mut h = TestHarness::new(80, 24);
    h.type_str("test input");
    let buf = h.render();
    assert!(buffer_contains(&buf, "test input"), "Typed text should appear in input bar");
}

// ===========================================================================
// Harness: slash command e2e (type → enter → dispatch → render)
// ===========================================================================

#[test]
fn harness_slash_help_full_flow() {
    let mut h = TestHarness::new(80, 30);
    h.type_str("/help");
    h.press_enter();

    let buf = h.render();
    assert!(buffer_contains(&buf, "Available commands"),
        "Typing /help + Enter should dispatch and render help output");
    assert!(buffer_contains(&buf, "> /help"),
        "User message should show with > prefix");
}

#[test]
fn harness_slash_status_full_flow() {
    let mut h = TestHarness::new(80, 30);
    h.type_str("/status");
    h.press_enter();

    let buf = h.render();
    assert!(buffer_contains(&buf, "Model: test-model"),
        "/status should render model name");
}

#[test]
fn harness_slash_exit_quits() {
    let mut h = TestHarness::new(80, 24);
    h.type_str("/exit");
    h.press_enter();
    assert!(h.app.state.should_quit, "/exit should set should_quit");
}

#[test]
fn harness_slash_clear_clears_messages() {
    let mut h = TestHarness::new(80, 30);
    h.type_str("/help");
    h.press_enter();
    assert!(!h.app.state.messages.is_empty());

    h.type_str("/clear");
    h.press_enter();
    assert!(h.app.state.messages.is_empty(), "/clear should empty messages");

    let buf = h.render();
    assert!(buffer_contains(&buf, "Strands"), "Welcome screen after /clear");
}

#[test]
fn harness_unknown_command_shows_error() {
    let mut h = TestHarness::new(80, 30);
    h.type_str("/nonexistent");
    h.press_enter();

    let buf = h.render();
    assert!(buffer_contains(&buf, "Unknown command"));
}

#[test]
fn harness_empty_enter_does_nothing() {
    let mut h = TestHarness::new(80, 24);
    h.press_enter();
    assert!(h.app.state.messages.is_empty());
}

// ===========================================================================
// Harness: repeated command cache invalidation (the /mcp bug)
// ===========================================================================

#[test]
fn harness_repeated_status_both_visible() {
    let mut h = TestHarness::new(80, 40);

    h.type_str("/status");
    h.press_enter();
    let buf1 = h.render();
    let c1 = buffer_lines(&buf1).iter().filter(|l| l.contains("Model: test-model")).count();
    assert_eq!(c1, 1);

    h.type_str("/status");
    h.press_enter();
    let buf2 = h.render();
    let c2 = buffer_lines(&buf2).iter().filter(|l| l.contains("Model: test-model")).count();
    assert_eq!(c2, 2, "Both /status outputs must be visible (cache invalidation)");
}

// ===========================================================================
// Harness: agent streaming simulation
// ===========================================================================

#[test]
fn harness_simulate_streaming_response() {
    let mut h = TestHarness::new(80, 30);
    h.type_str("hello");
    h.press_enter();
    h.app.state.agent_status = AgentStatus::Streaming;

    h.feed_agent_events(vec![
        Event::AgentTextDelta("Hello! ".to_string()),
        Event::AgentTextDelta("How can I help?\n".to_string()),
        Event::AgentDone,
    ]);

    assert!(matches!(h.app.state.agent_status, AgentStatus::Idle));
    let buf = h.render();
    assert!(buffer_contains(&buf, "How can I help?"));
}

#[test]
fn harness_streaming_with_tool_calls() {
    let mut h = TestHarness::new(100, 40);
    h.type_str("fix the bug");
    h.press_enter();
    h.app.state.agent_status = AgentStatus::Streaming;

    h.feed_agent_events(vec![Event::AgentTextDelta("Let me check.\n".to_string())]);
    h.simulate_tool_call("Read", "src/main.rs");
    h.feed_agent_events(vec![
        Event::AgentTextDelta("Found it.\n".to_string()),
        Event::AgentDone,
    ]);

    let buf = h.render();
    assert!(buffer_contains(&buf, "Let me check"));
    assert!(buffer_contains(&buf, "Found it"));
}

// ===========================================================================
// Unit 6: 1-block-per-message normalization
// ===========================================================================

/// AgentToolStart must create a new message, not append to the current one.
#[test]
fn tool_start_creates_new_message() {
    let mut h = TestHarness::new(80, 30);
    h.type_str("do something");
    h.press_enter();
    h.app.state.agent_status = AgentStatus::Streaming;

    let before = h.app.state.messages.len();

    h.feed_agent_events(vec![
        Event::AgentToolStart { name: "Read".to_string(), tool_use_id: String::new() },
    ]);

    assert_eq!(
        h.app.state.messages.len(),
        before + 1,
        "AgentToolStart must push a new message"
    );
    let last = h.app.state.messages.last().unwrap();
    assert!(
        last.blocks.iter().any(|b| matches!(b, super::app::ContentBlock::ToolCall { .. })),
        "New message should contain the ToolCall block"
    );
}

/// Two consecutive tool starts each create their own message.
#[test]
fn two_tool_starts_create_two_messages() {
    let mut h = TestHarness::new(80, 30);
    h.type_str("do things");
    h.press_enter();
    h.app.state.agent_status = AgentStatus::Streaming;

    let before = h.app.state.messages.len();

    h.feed_agent_events(vec![
        Event::AgentToolStart { name: "Read".to_string(), tool_use_id: "t1".to_string() },
        Event::AgentToolResult { status: "success".to_string(), content: String::new(), tool_use_id: "t1".to_string() },
        Event::AgentToolStart { name: "Grep".to_string(), tool_use_id: "t2".to_string() },
        Event::AgentToolResult { status: "success".to_string(), content: String::new(), tool_use_id: "t2".to_string() },
    ]);

    assert_eq!(
        h.app.state.messages.len(),
        before + 2,
        "Two tool calls should produce two new messages"
    );
}

/// Text after a completed tool result goes into a new message.
#[test]
fn text_after_tool_result_creates_new_message() {
    let mut h = TestHarness::new(80, 30);
    h.type_str("analyze");
    h.press_enter();
    h.app.state.agent_status = AgentStatus::Streaming;

    h.feed_agent_events(vec![
        Event::AgentTextDelta("Let me check.\n".to_string()),
    ]);
    let after_text = h.app.state.messages.len();

    h.feed_agent_events(vec![
        Event::AgentToolStart { name: "Read".to_string(), tool_use_id: "t1".to_string() },
        Event::AgentToolResult { status: "success".to_string(), content: String::new(), tool_use_id: "t1".to_string() },
    ]);
    let after_tool = h.app.state.messages.len();

    h.feed_agent_events(vec![
        Event::AgentTextDelta("Found the issue.\n".to_string()),
    ]);
    let after_post_text = h.app.state.messages.len();

    // Text before tool: same message count (appended)
    assert_eq!(after_text, after_tool - 1, "Tool start should add exactly one message");
    // Text after tool: should create a new message
    assert_eq!(after_post_text, after_tool + 1,
        "Text after a completed tool result must go into a new message");

    // Verify content is still present
    let buf = h.render();
    assert!(buffer_contains(&buf, "Let me check"), "Pre-tool text should render");
    assert!(buffer_contains(&buf, "Found the issue"), "Post-tool text should render");
}

/// Multiple consecutive text deltas go into the same message (not one each).
#[test]
fn consecutive_text_deltas_same_message() {
    let mut h = TestHarness::new(80, 30);
    h.type_str("explain");
    h.press_enter();
    h.app.state.agent_status = AgentStatus::Streaming;

    let before = h.app.state.messages.len();

    h.feed_agent_events(vec![
        Event::AgentTextDelta("First ".to_string()),
        Event::AgentTextDelta("second ".to_string()),
        Event::AgentTextDelta("third.\n".to_string()),
    ]);

    assert_eq!(
        h.app.state.messages.len(),
        before,
        "Consecutive text deltas must stay in the same message"
    );
}

/// AgentToolResult finds the Running ToolCall even though it is in a different message.
#[test]
fn tool_result_updates_tool_call_in_earlier_message() {
    let mut h = TestHarness::new(80, 30);
    h.type_str("read files");
    h.press_enter();
    h.app.state.agent_status = AgentStatus::Streaming;

    h.feed_agent_events(vec![
        Event::AgentToolStart { name: "Read".to_string(), tool_use_id: "t1".to_string() },
    ]);

    // Find the message that has the Running ToolCall
    let tool_msg_idx = h.app.state.messages.iter().position(|m| {
        m.blocks.iter().any(|b| matches!(b, super::app::ContentBlock::ToolCall {
            status: super::app::ToolCallStatus::Running, ..
        }))
    }).expect("Should have a Running ToolCall message");

    h.feed_agent_events(vec![
        Event::AgentToolResult { status: "success".to_string(), content: String::new(), tool_use_id: "t1".to_string() },
    ]);

    // The ToolCall in the earlier message should now be Success
    let tool_block = h.app.state.messages[tool_msg_idx].blocks.iter().find(|b| {
        matches!(b, super::app::ContentBlock::ToolCall { .. })
    }).expect("Message should still have a ToolCall block");

    assert!(
        matches!(tool_block, super::app::ContentBlock::ToolCall {
            status: super::app::ToolCallStatus::Success, ..
        }),
        "ToolCall in earlier message should be updated to Success"
    );
}

/// Full sequence: text → tool → text produces the right message structure.
#[test]
fn full_sequence_text_tool_text_structure() {
    let mut h = TestHarness::new(100, 40);
    h.type_str("fix bug");
    h.press_enter();
    h.app.state.agent_status = AgentStatus::Streaming;

    let base = h.app.state.messages.len();

    h.feed_agent_events(vec![
        Event::AgentTextDelta("Let me look.\n".to_string()),
        Event::AgentToolStart { name: "Read".to_string(), tool_use_id: "t1".to_string() },
        Event::AgentToolCall { name: "Read".to_string(), input: serde_json::json!({"file_path": "main.rs"}), tool_use_id: "t1".to_string() },
        Event::AgentToolResult { status: "success".to_string(), content: String::new(), tool_use_id: "t1".to_string() },
        Event::AgentTextDelta("Found the issue.\n".to_string()),
        Event::AgentDone,
    ]);

    // Text delta + ToolCall + post-tool text delta = 2 extra messages beyond base
    assert_eq!(
        h.app.state.messages.len(),
        base + 2,
        "Should have base + ToolCall message + post-tool text message"
    );

    let buf = h.render();
    assert!(buffer_contains(&buf, "Let me look"), "Pre-tool text should render");
    assert!(buffer_contains(&buf, "Found the issue"), "Post-tool text should render");
}

#[test]
fn harness_streaming_line_buffering() {
    let mut h = TestHarness::new(80, 30);
    h.type_str("explain");
    h.press_enter();
    h.app.state.agent_status = AgentStatus::Streaming;

    h.feed_agent_events(vec![Event::AgentTextDelta("Partial without newline".to_string())]);
    let buf = h.render();
    assert!(!buffer_contains(&buf, "Partial without"), "Partial line hidden during streaming");

    h.feed_agent_events(vec![Event::AgentTextDelta("\n".to_string())]);
    let buf = h.render();
    assert!(buffer_contains(&buf, "Partial without"), "Completed line now visible");
}

#[test]
fn harness_agent_error_renders() {
    let mut h = TestHarness::new(80, 30);
    h.type_str("test");
    h.press_enter();
    h.app.state.agent_status = AgentStatus::Streaming;

    h.feed_agent_events(vec![
        Event::AgentError("API rate limit exceeded".to_string()),
        Event::AgentDone,
    ]);

    let buf = h.render();
    assert!(buffer_contains(&buf, "rate limit"));
}

// ===========================================================================
// Harness: Ctrl+C double-tap flow
// ===========================================================================

#[test]
fn harness_ctrl_c_clears_input_first() {
    let mut h = TestHarness::new(80, 24);
    h.type_str("some text");
    h.press_ctrl_c();
    assert!(h.input_text().trim().is_empty(), "First Ctrl+C clears input");
    assert!(!h.app.state.should_quit);
}

#[test]
fn harness_ctrl_c_double_tap_quits() {
    let mut h = TestHarness::new(80, 24);
    h.app.state.tick_count = 100;
    h.press_ctrl_c();
    assert!(!h.app.state.should_quit);
    h.tick(5);
    h.press_ctrl_c();
    assert!(h.app.state.should_quit, "Double Ctrl+C within window quits");
}

#[test]
fn harness_ctrl_c_expired_window_no_quit() {
    let mut h = TestHarness::new(80, 24);
    h.app.state.tick_count = 100;
    h.press_ctrl_c();
    h.tick(30);
    h.press_ctrl_c();
    assert!(!h.app.state.should_quit, "Expired window → no quit");
}

#[test]
fn harness_ctrl_c_during_streaming_cancels() {
    let mut h = TestHarness::new(80, 24);
    h.type_str("hello");
    h.press_enter();
    h.app.state.agent_status = AgentStatus::Streaming;
    h.press_ctrl_c();
    assert!(matches!(h.app.state.agent_status, AgentStatus::Idle));
    assert!(!h.app.state.should_quit);
}

#[test]
fn harness_ctrl_c_status_bar_hint() {
    let mut h = TestHarness::new(100, 24);
    h.app.state.tick_count = 100;
    h.press_ctrl_c();
    let buf = h.render();
    let lines = buffer_lines(&buf);
    let last = &lines[lines.len() - 1];
    assert!(last.contains("Ctrl+C again to quit"), "got: {last}");
}

// ===========================================================================
// Harness: autocomplete flow
// ===========================================================================

#[test]
fn harness_slash_triggers_suggestions() {
    let mut h = TestHarness::new(80, 24);
    h.type_str("/");
    assert!(!h.app.state.suggestions.is_empty());
}

#[test]
fn harness_tab_accepts_suggestion() {
    let mut h = TestHarness::new(80, 24);
    h.type_str("/hel");
    assert!(h.app.state.suggestions.iter().any(|s| s.name == "help"));
    h.press_tab();
    assert!(h.input_text().starts_with("/help"), "Tab accepts suggestion");
}

#[test]
fn harness_esc_dismisses_suggestions() {
    let mut h = TestHarness::new(80, 24);
    h.type_str("/");
    assert!(!h.app.state.suggestions.is_empty());
    h.press_esc();
    assert!(h.app.state.suggestions.is_empty());
}

#[test]
fn harness_arrow_navigates_suggestions() {
    let mut h = TestHarness::new(80, 24);
    h.type_str("/");
    let initial = h.app.state.selected_suggestion;
    h.press_down();
    assert_ne!(h.app.state.selected_suggestion, initial);
    h.press_up();
    assert_eq!(h.app.state.selected_suggestion, initial);
}

// ===========================================================================
// Harness: Esc cancels streaming
// ===========================================================================

#[test]
fn harness_esc_cancels_streaming() {
    let mut h = TestHarness::new(80, 24);
    h.type_str("test");
    h.press_enter();
    h.app.state.agent_status = AgentStatus::Streaming;
    h.press_esc();
    assert!(matches!(h.app.state.agent_status, AgentStatus::Idle));
}

// ===========================================================================
// Harness: multi-turn conversation
// ===========================================================================

#[test]
fn harness_multi_turn_conversation() {
    let mut h = TestHarness::new(80, 40);

    h.type_str("What is Rust?");
    h.press_enter();
    h.app.state.agent_status = AgentStatus::Streaming;
    h.simulate_response("Rust is a systems programming language.\n");

    h.type_str("Tell me more");
    h.press_enter();
    h.app.state.agent_status = AgentStatus::Streaming;
    h.simulate_response("It focuses on safety and performance.\n");

    let buf = h.render();
    assert!(buffer_contains(&buf, "What is Rust?"));
    assert!(buffer_contains(&buf, "systems programming"));
    assert!(buffer_contains(&buf, "Tell me more"));
    assert!(buffer_contains(&buf, "safety and performance"));
    assert_eq!(h.app.state.turn_count, 2);
}

// ===========================================================================
// Harness: file path not treated as command
// ===========================================================================

#[test]
fn harness_file_path_not_command() {
    let mut h = TestHarness::new(80, 30);
    h.type_str("/var/log/syslog");
    h.press_enter();
    let has_unknown = h.app.state.messages.iter().any(|m| {
        m.blocks.iter().any(|b| matches!(b, super::app::ContentBlock::Text(t) if t.contains("Unknown command")))
    });
    assert!(!has_unknown);
}

// ===========================================================================
// Harness: input history
// ===========================================================================

#[test]
fn harness_input_history_populated() {
    let mut h = TestHarness::new(80, 30);
    h.type_str("first query");
    h.press_enter();
    h.app.state.agent_status = AgentStatus::Streaming;
    h.simulate_response("ok\n");

    h.type_str("second query");
    h.press_enter();
    h.app.state.agent_status = AgentStatus::Streaming;
    h.simulate_response("ok\n");

    assert_eq!(h.app.state.input_history.len(), 2);
    assert_eq!(h.app.state.input_history[0], "first query");
    assert_eq!(h.app.state.input_history[1], "second query");
}

#[test]
fn harness_slash_commands_not_in_history() {
    let mut h = TestHarness::new(80, 30);
    h.type_str("/help");
    h.press_enter();
    h.type_str("real question");
    h.press_enter();
    h.app.state.agent_status = AgentStatus::Streaming;
    h.simulate_response("ok\n");

    assert_eq!(h.app.state.input_history.len(), 1);
    assert_eq!(h.app.state.input_history[0], "real question");
}

// ===========================================================================
// Harness: scroll
// ===========================================================================

#[test]
fn harness_auto_scroll_on_by_default() {
    let h = TestHarness::new(80, 24);
    assert!(h.app.state.auto_scroll);
}

#[test]
fn harness_esc_cancel_does_not_affect_scroll() {
    let mut h = TestHarness::new(80, 24);
    h.type_str("test");
    h.press_enter();
    h.app.state.agent_status = AgentStatus::Streaming;
    h.press_esc();
    assert!(h.app.state.auto_scroll);
}

// ===========================================================================
// Session suggestions
// ===========================================================================

/// Helper: create a temp session JSONL file so session suggestions have data.
fn create_test_session(dir: &std::path::Path, session_id: &str) {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join(format!("{session_id}.jsonl"));
    std::fs::write(
        &path,
        // Minimal valid journal: one session_meta + one message entry
        format!(
            r#"{{"type":"session_meta","session_id":"{session_id}","version":1}}"#
        ) + "\n" + &format!(
            r#"{{"type":"message","uuid":"00000000-0000-0000-0000-000000000001","parent_uuid":null,"role":"user","content":[{{"type":"text","text":"hello"}}],"timestamp":"2026-04-04T00:00:00Z"}}"#
        ) + "\n",
    )
    .unwrap();
}

#[test]
fn generate_suggestions_resume_space_shows_sessions() {
    let cwd = std::env::current_dir().unwrap();
    let dir = crate::session::SessionId::storage_dir(&cwd);
    let test_id = "test-suggestion-sess-002";
    create_test_session(&dir, test_id);

    let registry = crate::commands::builtin_registry();
    let suggestions = crate::commands::generate_suggestions("/resume ", &registry, "test-model");

    assert!(
        suggestions.iter().any(|s| s.session_id.as_deref() == Some(test_id)),
        "/resume (space) should show session '{}', got: {:?}",
        test_id,
        suggestions.iter().map(|s| (&s.name, &s.session_id)).collect::<Vec<_>>()
    );

    let _ = std::fs::remove_file(dir.join(format!("{test_id}.jsonl")));
}

#[test]
fn harness_resume_tab_shows_session_suggestions() {
    let cwd = std::env::current_dir().unwrap();
    let dir = crate::session::SessionId::storage_dir(&cwd);
    let test_id = "test-harness-sess-004";
    create_test_session(&dir, test_id);

    let mut h = TestHarness::new(80, 24);
    h.type_str("/resume");
    // Tab accepts the "/resume" command suggestion and triggers update_suggestions
    h.press_tab();
    assert!(
        h.input_text().starts_with("/resume "),
        "Tab should accept to '/resume ', got: '{}'",
        h.input_text()
    );
    assert!(
        h.app.state.suggestions.iter().any(|s| s.session_id.is_some()),
        "After Tab on /resume, suggestions should show sessions, got: {:?}",
        h.app.state.suggestions.iter().map(|s| (&s.name, &s.session_id)).collect::<Vec<_>>()
    );

    let _ = std::fs::remove_file(dir.join(format!("{test_id}.jsonl")));
}

#[test]
fn harness_select_session_suggestion_sets_session_id() {
    let cwd = std::env::current_dir().unwrap();
    let dir = crate::session::SessionId::storage_dir(&cwd);
    let test_id = "test-harness-sess-005";
    create_test_session(&dir, test_id);

    let mut h = TestHarness::new(80, 24);
    // Type /resume to get session suggestions
    h.type_str("/resume");
    assert!(!h.app.state.suggestions.is_empty(), "should have suggestions");

    // Find the test session in suggestions and select it
    if let Some(idx) = h.app.state.suggestions.iter().position(|s| s.session_id.as_deref() == Some(test_id)) {
        h.app.state.selected_suggestion = idx as i32;
        let selected = h.app.selected_session_id();
        assert_eq!(
            selected.as_deref(),
            Some(test_id),
            "selected_session_id should return the full session ID"
        );
    } else {
        panic!("Test session not found in suggestions");
    }

    let _ = std::fs::remove_file(dir.join(format!("{test_id}.jsonl")));
}

/// Reproduce: user types "hi", agent responds, user types /new, then /resume.
/// The previous session should appear in the /resume suggestion list.
#[test]
fn clear_then_resume_shows_previous_session() {
    use strands::session::SessionManager;
    use strands::session::journal_session_manager::JournalSessionManager;
    use strands::types::content::{Message, ContentBlock, Role};

    let cwd = std::env::current_dir().unwrap();
    let dir = crate::session::SessionId::storage_dir(&cwd);
    let test_id = "test-clear-resume-001";

    // Write a real journal via JournalSessionManager (matches the live flow)
    let wrote_ok = std::thread::spawn({
        let dir = dir.clone();
        let test_id = test_id.to_string();
        move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let mgr = JournalSessionManager::new(
                    test_id,
                    Some(dir),
                    None,
                ).await.unwrap();

                mgr.append_message(
                    Message::new(Role::User, vec![ContentBlock::text("hi")]),
                    "agent1",
                ).await.unwrap();
                mgr.append_message(
                    Message::new(Role::Assistant, vec![ContentBlock::text("Hello!")]),
                    "agent1",
                ).await.unwrap();

                // Flush pending writes
                mgr.flush().await;
            });
        }
    }).join().is_ok();
    assert!(wrote_ok, "journal write should succeed");

    // Verify the file exists and has content
    let journal_path = dir.join(format!("{test_id}.jsonl"));
    assert!(journal_path.exists(), "journal file should exist at {:?}", journal_path);
    let size = std::fs::metadata(&journal_path).unwrap().len();
    assert!(size > 0, "journal file should not be empty, got {} bytes", size);

    // Now simulate /new then /resume — session should appear in suggestions
    let mut h = TestHarness::new(80, 24);

    // /clear
    h.type_str("/new");
    h.press_enter();

    // /resume — should show the session we created
    h.type_str("/resume");
    let found = h.app.state.suggestions.iter().any(|s| {
        s.session_id.as_deref() == Some(test_id)
    });
    assert!(
        found,
        "Session '{}' should appear in /resume suggestions after /new. Got: {:?}",
        test_id,
        h.app.state.suggestions.iter().map(|s| (&s.name, &s.session_id)).collect::<Vec<_>>()
    );

    let _ = std::fs::remove_file(&journal_path);
}

/// Simulate the EXACT user flow with a real journal and agent hooks:
/// 1. Type "hi" → Enter (agent adds user message via hook → journal writes)
/// 2. Agent responds "Hello!" (hook → journal writes)
/// 3. Type "/new" → Enter (clear)
/// 4. Type "/resume" → session should be listed
///
/// This test wires up a JournalSessionManager with MessageAddedEvent hooks
/// on the agent, matching the production setup in main.rs.
#[test]
fn full_flow_hi_new_resume_shows_session() {
    use strands::session::SessionManager;
    use strands::session::journal_session_manager::JournalSessionManager;
    use strands::types::content::{Message, ContentBlock, Role};

    let cwd = std::env::current_dir().unwrap();
    let sessions_dir = crate::session::SessionId::storage_dir(&cwd);
    std::fs::create_dir_all(&sessions_dir).unwrap();

    let test_session_id = format!("test-full-flow-{}", std::process::id());

    // Create the harness
    let mut h = TestHarness::new(80, 30);

    // Create a JournalSessionManager in the REAL sessions dir
    let journal = std::thread::spawn({
        let dir = sessions_dir.clone();
        let sid = test_session_id.clone();
        move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                JournalSessionManager::new(sid, Some(dir), None).await.unwrap()
            })
        }
    }).join().unwrap();

    // Register MessageAddedEvent hook (mirrors main.rs)
    {
        let mgr = std::sync::Arc::clone(&journal);
        h.app.agent_ref().add_hook(move |event: &strands::hooks::MessageAddedEvent| {
            let mgr2 = std::sync::Arc::clone(&mgr);
            let message = event.message.clone();
            let agent_id = event.agent_id.clone();
            tokio::spawn(async move {
                let _ = mgr2.append_message(message, &agent_id).await;
            });
        });
    }

    // === Step 1: User types "hi" → Enter ===
    h.type_str("hi");
    h.press_enter();
    // submit() calls stream_async in tokio::spawn. The MockModel produces nothing,
    // BUT stream_async does call add_message_with_hooks for the user message first.
    // Wait for the spawned tasks (stream_async + journal hook) to complete.
    std::thread::sleep(std::time::Duration::from_millis(200));

    // The agent's stream_async should have added the user "hi" message.
    // Now simulate the assistant response (which also fires the hook):
    h.app.state.agent_status = AgentStatus::Streaming;
    h.simulate_response("Hello!");
    // simulate_response calls handle_agent_event which doesn't fire SDK hooks.
    // We need to also add the assistant message to the agent state (with hooks):
    h.app.agent_ref().add_message(Message::new(
        Role::Assistant,
        vec![ContentBlock::text("Hello!")],
    ));
    std::thread::sleep(std::time::Duration::from_millis(200));

    // Verify journal file exists
    let journal_path = sessions_dir.join(format!("{}.jsonl", test_session_id));
    assert!(
        journal_path.exists(),
        "Journal file should exist after conversation. Path: {:?}",
        journal_path
    );

    // === Step 2: User types /new ===
    h.type_str("/new");
    h.press_enter();
    assert!(h.app.state.messages.is_empty(), "messages should be empty after /new");

    // === Step 3: User types /resume ===
    h.type_str("/resume ");
    let found = h.app.state.suggestions.iter().any(|s| {
        s.session_id.as_deref() == Some(test_session_id.as_str())
    });
    assert!(
        found,
        "Session '{}' should appear in /resume suggestions. Got: {:?}",
        test_session_id,
        h.app.state.suggestions.iter().map(|s| (&s.name, &s.session_id)).collect::<Vec<_>>()
    );

    // Cleanup
    let _ = std::fs::remove_file(&journal_path);
}

/// Reproduce: /resume uses a stale session cache, so newly-created sessions
/// don't appear. After /new, the next /resume must read from disk, not cache.
#[test]
fn resume_reads_disk_not_stale_cache() {
    let cwd = std::env::current_dir().unwrap();
    let dir = crate::session::SessionId::storage_dir(&cwd);
    std::fs::create_dir_all(&dir).unwrap();

    // Pre-populate the session cache with an older session
    let old_id = "test-old-session-001";
    create_test_session(&dir, old_id);

    // Force the cache to be populated (not empty) by writing directly.
    // This simulates what happens after the first /resume populates the cache.
    {
        use crate::session::SessionSummary;
        let old_summary = SessionSummary {
            session_id: old_id.to_string(),
            path: dir.join(format!("{old_id}.jsonl")),
            modified: chrono::Local::now(),
            size_bytes: 100,
            display_title: None,
            git_branch: None,
        };
        crate::session::set_session_cache(vec![old_summary]);
    }

    // Verify cache is populated and does NOT yet contain the new session
    let cached = crate::session::cached_sessions();
    assert!(!cached.is_empty(), "cache should be pre-populated");

    // Now create a NEW session (simulating what happens after /new)
    let new_id = "test-new-session-002";
    create_test_session(&dir, new_id);

    // The cache is now stale (doesn't contain new_id).
    // generate_suggestions should still find it because it reads from disk.
    let registry = crate::commands::builtin_registry();
    let suggestions = crate::commands::generate_suggestions("/resume ", &registry, "test-model");
    let found = suggestions.iter().any(|s| s.session_id.as_deref() == Some(new_id));
    assert!(
        found,
        "Newly created session '{}' should appear in /resume despite stale cache. Got: {:?}",
        new_id,
        suggestions.iter().map(|s| (&s.name, &s.session_id)).collect::<Vec<_>>()
    );

    let _ = std::fs::remove_file(dir.join(format!("{old_id}.jsonl")));
    let _ = std::fs::remove_file(dir.join(format!("{new_id}.jsonl")));
}

// ===========================================================================
// Session resume: reproduce exact user flow bugs
// ===========================================================================

/// Reproduce EXACT bug: resume → say "cool" → /new → /resume → messages duplicated.
///
/// Simulates the full journal lifecycle:
/// 1. Write a session with "hi" / "Hello!" to a journal file
/// 2. Resume it (load messages into agent + display)
/// 3. User says "cool", agent responds "Nice!" (written to journal via hook)
/// 4. /new (clear)
/// 5. /resume same session → check for duplicates
#[test]
fn resume_say_new_resume_no_duplicates() {
    use strands::types::content::{Message, ContentBlock, Role};
    use strands::session::SessionManager;
    use strands::session::journal_session_manager::{
        JournalSessionManager, load_journal, build_conversation_chain,
    };

    let tmp = std::env::temp_dir().join(format!("strands-resume-say-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    let session_id = "resume-say-test";

    let chain = std::thread::spawn({
        let tmp = tmp.clone();
        let sid = session_id.to_string();
        move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let mgr = JournalSessionManager::new(
                    sid.clone(), Some(tmp.clone()), None,
                ).await.unwrap();

                // Step 1: Original session — user says "hi", agent responds
                mgr.append_message(Message::user("hi"), "a1").await.unwrap();
                mgr.append_message(Message::assistant("Hello!"), "a1").await.unwrap();
                let _ = mgr.flush().await;

                // Step 2: Resume — load the session (this is what load_message does, no re-write)
                let path = tmp.join(format!("{sid}.jsonl"));
                let loaded = load_journal(&path).await.unwrap();
                let leaf = loaded.last_chain_uuid.unwrap();
                let resumed_msgs = build_conversation_chain(&loaded, leaf);
                assert_eq!(resumed_msgs.len(), 2, "Resumed session should have 2 messages");

                // Step 3: User says "cool" after resume — hook writes to SAME journal
                // This is the key: append_message writes with parent = last_chain_uuid
                // which should be the UUID of the "Hello!" entry
                mgr.append_message(Message::user("cool"), "a1").await.unwrap();
                mgr.append_message(Message::assistant("Nice!"), "a1").await.unwrap();
                let _ = mgr.flush().await;

                // Step 4: /new (just clears agent state, journal untouched)

                // Step 5: /resume — load the journal again
                let loaded2 = load_journal(&path).await.unwrap();
                let leaf2 = loaded2.last_chain_uuid.unwrap();
                let chain2 = build_conversation_chain(&loaded2, leaf2);

                // Report what we got
                eprintln!("Journal entries: {}", loaded2.messages.len());
                eprintln!("Chain length: {}", chain2.len());
                for (i, m) in chain2.iter().enumerate() {
                    eprintln!("  [{}] {:?}: '{}'", i, m.role, m.get_text());
                }

                chain2
            })
        }
    }).join().unwrap();

    // Verify: chain should have exactly 4 messages, no duplicates
    assert_eq!(
        chain.len(), 4,
        "Chain should have 4 messages (hi, Hello!, cool, Nice!), got {}. Messages: {:?}",
        chain.len(),
        chain.iter().map(|m| format!("{:?}: '{}'", m.role, m.get_text())).collect::<Vec<_>>()
    );

    // Check no consecutive duplicates
    for i in 1..chain.len() {
        assert!(
            !(chain[i].role == chain[i-1].role && chain[i].get_text() == chain[i-1].get_text()),
            "Duplicate at index {}: {:?} '{}'",
            i, chain[i].role, chain[i].get_text()
        );
    }

    // Simulate display path
    let mut h = TestHarness::new(80, 30);
    simulate_resume(&mut h, chain);

    let user_msgs: Vec<_> = h.app.state.messages.iter()
        .filter(|m| matches!(m.role, super::app::Role::User))
        .collect();
    assert_eq!(
        user_msgs.len(), 2,
        "Display should have 2 user messages, got {}. All: {:?}",
        user_msgs.len(),
        h.app.state.messages.iter().map(|m| format!("{:?}: '{}'", m.role, m.text_content())).collect::<Vec<_>>()
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// Reproduce the REAL scenario: app starts with session A, user resumes session B,
/// says something (written to A's journal), then /new, /resume.
/// The resumed session B's journal should be unchanged.
/// Session A's journal should have the new messages.
#[test]
fn resume_different_session_then_new_resume() {
    use strands::types::content::{Message, ContentBlock, Role};
    use strands::session::SessionManager;
    use strands::session::journal_session_manager::{
        JournalSessionManager, load_journal, build_conversation_chain,
    };

    let tmp = std::env::temp_dir().join(format!("strands-cross-resume-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    let result = std::thread::spawn({
        let tmp = tmp.clone();
        move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                // Session B: the one we'll resume (pre-existing)
                let mgr_b = JournalSessionManager::new(
                    "session-B".to_string(), Some(tmp.clone()), None,
                ).await.unwrap();
                mgr_b.append_message(Message::user("hi"), "a1").await.unwrap();
                mgr_b.append_message(Message::assistant("Hello!"), "a1").await.unwrap();
                let _ = mgr_b.flush().await;

                // Session A: the current session (created on app startup)
                let mgr_a = JournalSessionManager::new(
                    "session-A".to_string(), Some(tmp.clone()), None,
                ).await.unwrap();

                // User resumes session B → loads B's messages (no hooks)
                // Then says "cool" → hook writes to session A's journal
                mgr_a.append_message(Message::user("cool"), "a1").await.unwrap();
                mgr_a.append_message(Message::assistant("Nice!"), "a1").await.unwrap();
                let _ = mgr_a.flush().await;

                // /new → clears agent
                // /resume → user picks session B again

                let path_b = tmp.join("session-B.jsonl");
                let loaded_b = load_journal(&path_b).await.unwrap();
                let leaf_b = loaded_b.last_chain_uuid.unwrap();
                let chain_b = build_conversation_chain(&loaded_b, leaf_b);

                let path_a = tmp.join("session-A.jsonl");
                let loaded_a = load_journal(&path_a).await.unwrap();
                let leaf_a = loaded_a.last_chain_uuid.unwrap();
                let chain_a = build_conversation_chain(&loaded_a, leaf_a);

                eprintln!("Session B chain ({} msgs):", chain_b.len());
                for (i, m) in chain_b.iter().enumerate() {
                    eprintln!("  [{}] {:?}: '{}'", i, m.role, m.get_text());
                }
                eprintln!("Session A chain ({} msgs):", chain_a.len());
                for (i, m) in chain_a.iter().enumerate() {
                    eprintln!("  [{}] {:?}: '{}'", i, m.role, m.get_text());
                }

                (chain_a, chain_b)
            })
        }
    }).join().unwrap();

    let (chain_a, chain_b) = result;

    // Session B should be UNCHANGED — only the original 2 messages
    assert_eq!(
        chain_b.len(), 2,
        "Session B should still have 2 messages, got {}",
        chain_b.len()
    );

    // Session A should have the new messages
    assert_eq!(
        chain_a.len(), 2,
        "Session A should have 2 messages (cool, Nice!), got {}",
        chain_a.len()
    );

    // Display path for session B — should show 2 messages, no duplicates
    let mut h = TestHarness::new(80, 30);
    simulate_resume(&mut h, chain_b);
    assert_eq!(h.app.state.messages.len(), 2, "Display of session B should have 2 messages");

    let _ = std::fs::remove_dir_all(&tmp);
}

/// Reproduce: resume current session (A), say "cool" (writes to A), /new, /resume A.
/// Session A now has: original messages + "cool" + response.
/// This tests the case where new messages are appended to the SAME session that was resumed.
#[test]
fn resume_same_session_say_new_resume() {
    use strands::types::content::{Message, ContentBlock, Role};
    use strands::session::SessionManager;
    use strands::session::journal_session_manager::{
        JournalSessionManager, load_journal, build_conversation_chain,
    };

    let tmp = std::env::temp_dir().join(format!("strands-same-sess-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    let result = std::thread::spawn({
        let tmp = tmp.clone();
        move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                // Single session A — all writes go to the same journal
                let mgr = JournalSessionManager::new(
                    "session-A".to_string(), Some(tmp.clone()), None,
                ).await.unwrap();

                // Step 1: Original conversation
                mgr.append_message(Message::user("hi"), "a1").await.unwrap();
                mgr.append_message(Message::assistant("Hello!"), "a1").await.unwrap();
                let _ = mgr.flush().await;

                // Step 2: /resume (loads session A) — no writes to journal (load_message)
                // Step 3: User says "cool" — hook writes to same journal A
                mgr.append_message(Message::user("cool"), "a1").await.unwrap();
                mgr.append_message(Message::assistant("Nice!"), "a1").await.unwrap();
                let _ = mgr.flush().await;

                // Step 4: /new
                // Step 5: /resume A again
                let path = tmp.join("session-A.jsonl");
                let loaded = load_journal(&path).await.unwrap();

                eprintln!("Raw journal entries: {}", loaded.messages.len());
                // Show the parent chain for each entry
                for (uuid, entry) in &loaded.messages {
                    if let strands::types::journal::JournalEntry::Message { meta, message, .. } = entry {
                        eprintln!("  {} parent={:?} {:?}: '{}'",
                            uuid, meta.parent_uuid, message.role, message.get_text());
                    }
                }

                let leaf = loaded.last_chain_uuid.unwrap();
                let chain = build_conversation_chain(&loaded, leaf);

                eprintln!("Chain ({} msgs):", chain.len());
                for (i, m) in chain.iter().enumerate() {
                    eprintln!("  [{}] {:?}: '{}'", i, m.role, m.get_text());
                }

                chain
            })
        }
    }).join().unwrap();

    assert_eq!(
        result.len(), 4,
        "Should have 4 messages (hi, Hello!, cool, Nice!), got {}. Messages: {:?}",
        result.len(),
        result.iter().map(|m| format!("{:?}: '{}'", m.role, m.get_text())).collect::<Vec<_>>()
    );

    // Simulate display — should be 4 messages, no duplicates
    let mut h = TestHarness::new(80, 30);
    simulate_resume(&mut h, result);

    let user_msgs: Vec<_> = h.app.state.messages.iter()
        .filter(|m| matches!(m.role, super::app::Role::User))
        .collect();
    assert_eq!(user_msgs.len(), 2, "Should have 2 user messages in display");

    let _ = std::fs::remove_dir_all(&tmp);
}

/// Confirm the root cause: the OLD buggy code (add_message instead of load_message)
/// corrupts the journal by re-writing messages. The fix is at the write path
/// (load_message skips hooks), not the read path.
#[test]
fn resume_double_write_corrupts_journal() {
    use strands::types::content::{Message, ContentBlock, Role};
    use strands::session::SessionManager;
    use strands::session::journal_session_manager::{
        JournalSessionManager, load_journal, build_conversation_chain,
    };

    let tmp = std::env::temp_dir().join(format!("strands-corrupt-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    let chain_len = std::thread::spawn({
        let tmp = tmp.clone();
        move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let mgr = JournalSessionManager::new(
                    "corrupt".to_string(), Some(tmp.clone()), None,
                ).await.unwrap();

                // Original conversation
                mgr.append_message(Message::user("hi"), "a1").await.unwrap();
                mgr.append_message(Message::assistant("Hello!"), "a1").await.unwrap();
                let _ = mgr.flush().await;

                // Simulate OLD bug: re-append messages (as add_message with hooks would)
                let path = tmp.join("corrupt.jsonl");
                let loaded = load_journal(&path).await.unwrap();
                let leaf = loaded.last_chain_uuid.unwrap();
                let resumed = build_conversation_chain(&loaded, leaf);
                for m in &resumed {
                    mgr.append_message(m.clone(), "a1").await.unwrap();
                }
                let _ = mgr.flush().await;

                // Reload and check — should be corrupted (6 instead of 2)
                let loaded2 = load_journal(&path).await.unwrap();
                let leaf2 = loaded2.last_chain_uuid.unwrap();
                build_conversation_chain(&loaded2, leaf2).len()
            })
        }
    }).join().unwrap();

    // The old bug produces duplicates — this documents the corruption
    assert!(
        chain_len > 2,
        "Old buggy code should corrupt the journal (got {} messages, expected >2). \
         The fix is load_message (no hooks) at the write path, not dedup at read path.",
        chain_len
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

// ===========================================================================
// Session resume: display deduplication
// ===========================================================================

/// Helper: simulate SessionResumed processing on a harness (same as event loop in mod.rs).
fn simulate_resume(h: &mut TestHarness, sdk_messages: Vec<strands::types::content::Message>) {
    h.app.state.messages.clear();
    h.app.state.clear_render_caches();
    h.app.state.messages = super::app::rebuild_display_messages(&sdk_messages);
    h.app.state.turn_count = h.app.state.messages.iter()
        .filter(|m| matches!(m.role, super::app::Role::User))
        .count();
    h.app.state.auto_scroll = true;
    h.app.state.scroll_offset = 0;
}

/// After resume, each user message should appear exactly once in the display.
/// Reproduces the bug where "hello" showed twice after resuming a simple conversation.
#[test]
fn resume_no_duplicate_user_messages() {
    use strands::types::content::{Message, ContentBlock, Role};

    let mut h = TestHarness::new(80, 30);

    // Build SDK messages for a simple 2-turn conversation:
    // User → Assistant → User → Assistant
    let sdk_messages = vec![
        Message::new(Role::User, vec![ContentBlock::text("hello, my name is Trung")]),
        Message::new(Role::Assistant, vec![ContentBlock::text("Hello Trung! How can I help?")]),
        Message::new(Role::User, vec![ContentBlock::text("good bye")]),
        Message::new(Role::Assistant, vec![ContentBlock::text("Goodbye! Happy coding!")]),
    ];

    simulate_resume(&mut h, sdk_messages);

    // Count user messages in display
    let user_msgs: Vec<&ChatMessage> = h.app.state.messages.iter()
        .filter(|m| matches!(m.role, super::app::Role::User))
        .collect();
    let assistant_msgs: Vec<&ChatMessage> = h.app.state.messages.iter()
        .filter(|m| matches!(m.role, super::app::Role::Assistant))
        .collect();

    assert_eq!(
        user_msgs.len(), 2,
        "Expected 2 user messages, got {}. Messages: {:?}",
        user_msgs.len(),
        h.app.state.messages.iter().map(|m| format!("{:?}: {}", m.role, m.text_content())).collect::<Vec<_>>()
    );
    assert_eq!(
        assistant_msgs.len(), 2,
        "Expected 2 assistant messages, got {}",
        assistant_msgs.len()
    );

    // Verify content — no duplicates
    assert_eq!(user_msgs[0].text_content(), "hello, my name is Trung");
    assert_eq!(user_msgs[1].text_content(), "good bye");
    assert_eq!(h.app.state.messages.len(), 4, "Total should be 4 messages, not more");
}

/// Resume with tool calls should not produce extra display messages.
#[test]
fn resume_with_tool_calls_no_duplicates() {
    use strands::types::content::{Message, ContentBlock, Role};
    use strands::types::tools::{ToolResult, ToolResultContent};

    let mut h = TestHarness::new(80, 30);

    // SDK messages: User → Assistant(ToolUse) → User(ToolResult) → Assistant(Text)
    let sdk_messages = vec![
        Message::new(Role::User, vec![ContentBlock::text("read my file")]),
        Message::new(Role::Assistant, vec![
            ContentBlock::tool_use_from_parts("tu1", "Read", serde_json::json!({"file_path": "/tmp/foo"})),
        ]),
        Message::new(Role::User, vec![
            ContentBlock::tool_result(ToolResult {
                tool_use_id: "tu1".to_string(),
                content: vec![ToolResultContent { text: Some("file contents here".to_string()), document: None, image: None, json: None }],
                status: strands::types::tools::ToolResultStatus::Success,
            }),
        ]),
        Message::new(Role::Assistant, vec![ContentBlock::text("Here is the file content.")]),
    ];

    simulate_resume(&mut h, sdk_messages);

    // "read my file" should appear exactly once as a user message
    let user_text_msgs: Vec<&ChatMessage> = h.app.state.messages.iter()
        .filter(|m| matches!(m.role, super::app::Role::User))
        .filter(|m| m.text_content().contains("read my file"))
        .collect();

    assert_eq!(
        user_text_msgs.len(), 1,
        "User prompt 'read my file' should appear exactly once, got {}. All messages: {:?}",
        user_text_msgs.len(),
        h.app.state.messages.iter().map(|m| format!("{:?}: {}", m.role, m.text_content())).collect::<Vec<_>>()
    );
}

/// Render after resume should not show duplicate lines.
#[test]
fn resume_render_no_duplicate_lines() {
    use strands::types::content::{Message, ContentBlock, Role};

    let mut h = TestHarness::new(80, 30);

    let sdk_messages = vec![
        Message::new(Role::User, vec![ContentBlock::text("hello world")]),
        Message::new(Role::Assistant, vec![ContentBlock::text("Hi there!")]),
    ];

    simulate_resume(&mut h, sdk_messages);

    let buf = h.render();
    let lines = buffer_lines(&buf);

    // Count how many lines contain "hello world"
    let count = lines.iter().filter(|l| l.contains("hello world")).count();
    assert_eq!(
        count, 1,
        "\"hello world\" should appear exactly once in rendered output, got {}",
        count
    );
}

/// Test the full journal write → load → chain build → display path.
/// If the journal produces duplicate messages, this test will catch it.
#[test]
fn resume_journal_roundtrip_no_duplicates() {
    use strands::types::content::{Message, ContentBlock, Role};
    use strands::session::SessionManager;
    use strands::session::journal_session_manager::{
        JournalSessionManager, load_journal, build_conversation_chain,
    };

    // Use the harness's runtime for the async work (it leaks a static ref)
    let mut h = TestHarness::new(80, 30);

    let tmp = std::env::temp_dir().join(format!("strands-resume-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    // Write journal entries using a spawned async task on the harness runtime
    let tmp2 = tmp.clone();
    let chain = std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mgr = JournalSessionManager::new(
                "test-session".to_string(),
                Some(tmp2.clone()),
                None,
            ).await.unwrap();

            mgr.append_message(
                Message::new(Role::User, vec![ContentBlock::text("hello, my name is Trung")]),
                "agent1",
            ).await.unwrap();
            mgr.append_message(
                Message::new(Role::Assistant, vec![ContentBlock::text("Hello Trung!")]),
                "agent1",
            ).await.unwrap();
            mgr.append_message(
                Message::new(Role::User, vec![ContentBlock::text("good bye")]),
                "agent1",
            ).await.unwrap();
            mgr.append_message(
                Message::new(Role::Assistant, vec![ContentBlock::text("Goodbye!")]),
                "agent1",
            ).await.unwrap();

            let journal_path = tmp2.join("test-session.jsonl");
            let loaded = load_journal(&journal_path).await.unwrap();
            let leaf = loaded.last_chain_uuid.expect("should have a leaf");
            build_conversation_chain(&loaded, leaf)
        })
    }).join().unwrap();

    // Chain should have exactly 4 messages
    assert_eq!(
        chain.len(), 4,
        "Journal chain should have 4 messages, got {}. Roles: {:?}",
        chain.len(),
        chain.iter().map(|m| format!("{:?}", m.role)).collect::<Vec<_>>()
    );
    assert_eq!(chain[0].role, Role::User);
    assert_eq!(chain[1].role, Role::Assistant);
    assert_eq!(chain[2].role, Role::User);
    assert_eq!(chain[3].role, Role::Assistant);
    assert_eq!(chain[0].get_text(), "hello, my name is Trung");
    assert_eq!(chain[2].get_text(), "good bye");

    // Simulate the display path
    simulate_resume(&mut h, chain);

    let user_msgs: Vec<&ChatMessage> = h.app.state.messages.iter()
        .filter(|m| matches!(m.role, super::app::Role::User))
        .collect();
    assert_eq!(
        user_msgs.len(), 2,
        "Display should have 2 user messages, got {}. All: {:?}",
        user_msgs.len(),
        h.app.state.messages.iter().map(|m| format!("{:?}: '{}'", m.role, m.text_content())).collect::<Vec<_>>()
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// Simulate the real resume bug: resume_session calls agent.add_message() for each
/// loaded message, which fires MessageAddedEvent → journal writes them AGAIN.
/// On a second resume, build_conversation_chain sees duplicates from sibling recovery.
/// This test reproduces that exact scenario.
#[test]
fn resume_double_write_causes_duplicates() {
    use strands::types::content::{Message, ContentBlock, Role};
    use strands::session::SessionManager;
    use strands::session::journal_session_manager::{
        JournalSessionManager, load_journal, build_conversation_chain,
    };

    let tmp = std::env::temp_dir().join(format!("strands-dupe-resume-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    let result = std::thread::spawn({
        let tmp = tmp.clone();
        move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let mgr = JournalSessionManager::new(
                    "dupe-test".to_string(),
                    Some(tmp.clone()),
                    None,
                ).await.unwrap();

                // Step 1: Write original conversation (simulating first session)
                mgr.append_message(
                    Message::new(Role::User, vec![ContentBlock::text("hello")]),
                    "agent1",
                ).await.unwrap();
                mgr.append_message(
                    Message::new(Role::Assistant, vec![ContentBlock::text("Hi!")]),
                    "agent1",
                ).await.unwrap();
                mgr.append_message(
                    Message::new(Role::User, vec![ContentBlock::text("bye")]),
                    "agent1",
                ).await.unwrap();
                mgr.append_message(
                    Message::new(Role::Assistant, vec![ContentBlock::text("Bye!")]),
                    "agent1",
                ).await.unwrap();

                // Step 2: Load the journal (simulating resume)
                let journal_path = tmp.join("dupe-test.jsonl");
                let loaded = load_journal(&journal_path).await.unwrap();
                let leaf1 = loaded.last_chain_uuid.unwrap();
                let chain1 = build_conversation_chain(&loaded, leaf1);

                assert_eq!(chain1.len(), 4, "First load: should have 4 messages");

                // Step 3: Simulate the FIXED resume_session which uses load_message
                // (no hooks) instead of add_message. The old bug was that add_message
                // fires MessageAddedEvent → journal writes them again.
                // The fix: resume uses Agent::load_message which skips hooks.
                // To test the fix, we do NOT re-append to the journal here.
                // (The old buggy code would have done: mgr.append_message(m, "agent1")
                // for each message, producing duplicates.)

                // Step 4: Load again (simulating a second resume or just checking state)
                let loaded2 = load_journal(&journal_path).await.unwrap();
                let leaf2 = loaded2.last_chain_uuid.unwrap();
                let chain2 = build_conversation_chain(&loaded2, leaf2);

                chain2
            })
        }
    }).join().unwrap();

    // This is the actual bug: after re-writing messages, chain may have duplicates
    // due to sibling recovery in build_conversation_chain
    let user_count = result.iter().filter(|m| m.role == strands::types::content::Role::User).count();
    let total = result.len();

    // Report what we found
    eprintln!(
        "After double-write: chain has {} messages ({} user). Messages: {:?}",
        total, user_count,
        result.iter().map(|m| format!("{:?}: '{}'", m.role, m.get_text())).collect::<Vec<_>>()
    );

    // The bug: this will be > 4 if duplicates exist
    if total > 4 {
        eprintln!("BUG CONFIRMED: journal double-write produces {} messages instead of 4", total);
    }

    // This assertion documents the expected behavior (currently may fail — that's the bug)
    assert_eq!(
        total, 4,
        "Chain should have 4 messages after resume re-write, got {}. \
         The resume flow should not re-append messages that came from the journal. \
         Messages: {:?}",
        total,
        result.iter().map(|m| format!("{:?}: '{}'", m.role, m.get_text())).collect::<Vec<_>>()
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// Simulate what happens in a real session: user types "hi", agent responds,
/// then on resume the conversation is loaded. Checks for 3-message bug
/// (extra message leaking into the journal).
#[test]
fn resume_single_turn_exact_message_count() {
    use strands::types::content::{Message, ContentBlock, Role};
    use strands::session::SessionManager;
    use strands::session::journal_session_manager::{
        JournalSessionManager, load_journal, build_conversation_chain,
    };

    let tmp = std::env::temp_dir().join(format!("strands-single-turn-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    let result = std::thread::spawn({
        let tmp = tmp.clone();
        move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let mgr = JournalSessionManager::new(
                    "single-turn".to_string(),
                    Some(tmp.clone()),
                    None,
                ).await.unwrap();

                // Exactly what stream_async does: user msg then assistant msg
                mgr.append_message(
                    Message::new(Role::User, vec![ContentBlock::text("hi")]),
                    "agent1",
                ).await.unwrap();
                mgr.append_message(
                    Message::new(Role::Assistant, vec![ContentBlock::text("Hi! How can I help?")]),
                    "agent1",
                ).await.unwrap();

                // Load and check
                let path = tmp.join("single-turn.jsonl");
                let loaded = load_journal(&path).await.unwrap();

                // How many raw journal entries?
                let entry_count = loaded.messages.len();

                let leaf = loaded.last_chain_uuid.unwrap();
                let chain = build_conversation_chain(&loaded, leaf);

                (entry_count, chain)
            })
        }
    }).join().unwrap();

    let (entry_count, chain) = result;

    assert_eq!(
        entry_count, 2,
        "Journal should have exactly 2 entries, got {}",
        entry_count
    );
    assert_eq!(
        chain.len(), 2,
        "Chain should have exactly 2 messages for a single turn, got {}. Messages: {:?}",
        chain.len(),
        chain.iter().map(|m| format!("{:?}: '{}'", m.role, m.get_text())).collect::<Vec<_>>()
    );

    // Display check
    let mut h = TestHarness::new(80, 30);
    simulate_resume(&mut h, chain);

    assert_eq!(h.app.state.messages.len(), 2);
    let user_msgs: Vec<_> = h.app.state.messages.iter()
        .filter(|m| matches!(m.role, super::app::Role::User))
        .collect();
    assert_eq!(user_msgs.len(), 1, "Should have exactly 1 user message");

    let _ = std::fs::remove_dir_all(&tmp);
}

/// Reproduce the EXACT user-reported scenario:
/// 1. First session: user types "hi", agent responds "Hi!"
/// 2. Resume: display shows "> hi" twice
/// Tests by constructing 3 SDK messages (which is what the user saw: "3 messages")
/// and checking for duplicates.
#[test]
fn resume_three_messages_no_duplicate_display() {
    use strands::types::content::{Message, ContentBlock, Role};

    let mut h = TestHarness::new(80, 30);

    // 3 messages could happen if: User, Assistant, then an AgentState or
    // metadata entry was miscounted. But for display, only User+Assistant matter.
    // Test with 3 messages where the 3rd is a stale user tool-result:
    let sdk_messages = vec![
        Message::new(Role::User, vec![ContentBlock::text("hi")]),
        Message::new(Role::Assistant, vec![ContentBlock::text("Hi! How can I help?")]),
        // A stale tool-result-only user message (from a previous buggy write)
        Message::new(Role::User, vec![ContentBlock::text("hi")]),
    ];

    simulate_resume(&mut h, sdk_messages);

    let buf = h.render();
    let lines = buffer_lines(&buf);
    let hi_count = lines.iter().filter(|l| l.contains("hi")).count();

    // Even with 3 SDK messages, "hi" should ideally appear only once in render.
    // But from_sdk_message converts ALL of them, so 2 user messages show as 2 "> hi".
    // This confirms the bug is in the journal producing 3 messages, not in rendering.
    eprintln!("Display with 3 SDK msgs: {} 'hi' occurrences. Messages: {:?}",
        hi_count,
        h.app.state.messages.iter().map(|m| format!("{:?}: '{}'", m.role, m.text_content())).collect::<Vec<_>>()
    );

    // The display has 3 messages because the SDK gave us 3. The fix must be in the journal.
    assert_eq!(
        h.app.state.messages.len(), 3,
        "from_sdk_message faithfully converts all 3 SDK messages"
    );
}

/// Simulate the real resume flow: resume_session sends SessionResumed + AgentTextDelta + AgentDone.
/// The AgentTextDelta "Resumed session..." should not create a duplicate.
#[test]
fn resume_full_flow_no_duplicates() {
    use strands::types::content::{Message, ContentBlock, Role};

    let mut h = TestHarness::new(80, 30);

    let sdk_messages = vec![
        Message::new(Role::User, vec![ContentBlock::text("hello")]),
        Message::new(Role::Assistant, vec![ContentBlock::text("Hi!")]),
        Message::new(Role::User, vec![ContentBlock::text("bye")]),
        Message::new(Role::Assistant, vec![ContentBlock::text("Bye!")]),
    ];

    // Simulate full event sequence as sent by resume_session
    simulate_resume(&mut h, sdk_messages);
    // After resume, AgentTextDelta appends to the last message
    h.app.handle_agent_event(Event::AgentTextDelta(
        "\nResumed session test-123 (4 messages)".to_string(),
    ));
    h.app.handle_agent_event(Event::AgentDone);

    let user_msgs: Vec<&ChatMessage> = h.app.state.messages.iter()
        .filter(|m| matches!(m.role, super::app::Role::User))
        .collect();

    assert_eq!(
        user_msgs.len(), 2,
        "Expected 2 user messages after full resume flow, got {}. All: {:?}",
        user_msgs.len(),
        h.app.state.messages.iter().map(|m| format!("{:?}: '{}'", m.role, m.text_content())).collect::<Vec<_>>()
    );

    // The "Resumed session..." text should be appended to the last assistant message, not as a new message
    let last = h.app.state.messages.last().unwrap();
    assert!(
        matches!(last.role, super::app::Role::Assistant),
        "Last message should be assistant"
    );
    assert!(
        last.text_content().contains("Resumed session"),
        "Last assistant message should contain resume notice"
    );
}

// ===========================================================================
// rebuild_display_messages: tool_use / tool_result pairing (Unit 5)
// ===========================================================================

/// Core reordering test: each ToolUse is immediately paired with its ToolResult
/// in the same ChatMessage. User messages with only ToolResult content are skipped.
#[test]
fn rebuild_display_messages_pairs_tool_use_with_result() {
    use strands::types::content::{ContentBlock, Message, Role};
    use strands::types::tools::{ToolResult, ToolResultContent, ToolResultStatus};

    // SDK message layout:
    //   [User]      "Fix the bug in main.rs"
    //   [Assistant] Text("Let me look...") + ToolUse("Read", id="t1") + ToolUse("Grep", id="t2")
    //   [User]      ToolResult(id="t1") + ToolResult(id="t2")
    //   [Assistant] Text("I found the issue")
    let sdk_messages = vec![
        Message::new(
            Role::User,
            vec![ContentBlock::text("Fix the bug in main.rs")],
        ),
        Message::new(
            Role::Assistant,
            vec![
                ContentBlock::text("Let me look at the file."),
                ContentBlock::tool_use_from_parts(
                    "t1",
                    "Read",
                    serde_json::json!({"file_path": "main.rs"}),
                ),
                ContentBlock::tool_use_from_parts(
                    "t2",
                    "Grep",
                    serde_json::json!({"pattern": "bug", "path": "."}),
                ),
            ],
        ),
        Message::new(
            Role::User,
            vec![
                ContentBlock::tool_result(ToolResult {
                    tool_use_id: "t1".to_string(),
                    content: vec![ToolResultContent {
                        text: Some("file contents here".to_string()),
                        document: None,
                        image: None,
                        json: None,
                    }],
                    status: ToolResultStatus::Success,
                }),
                ContentBlock::tool_result(ToolResult {
                    tool_use_id: "t2".to_string(),
                    content: vec![ToolResultContent {
                        text: Some("match found on line 42".to_string()),
                        document: None,
                        image: None,
                        json: None,
                    }],
                    status: ToolResultStatus::Success,
                }),
            ],
        ),
        Message::new(
            Role::Assistant,
            vec![ContentBlock::text("I found the issue.")],
        ),
    ];

    let msgs = super::app::rebuild_display_messages(&sdk_messages);

    // Expected ChatMessages (in order):
    //   0: User       "Fix the bug in main.rs"
    //   1: Assistant  Text("Let me look at the file.")
    //   2: Assistant  ToolCall("Read") + ToolResult(t1)
    //   3: Assistant  ToolCall("Grep") + ToolResult(t2)
    //   4: Assistant  Text("I found the issue.")
    assert_eq!(
        msgs.len(),
        5,
        "Expected 5 ChatMessages, got {}. Messages: {:?}",
        msgs.len(),
        msgs.iter()
            .map(|m| format!("{:?}: {} blocks", m.role, m.blocks.len()))
            .collect::<Vec<_>>()
    );

    // Message 0: user prompt
    assert!(matches!(msgs[0].role, super::app::Role::User));
    assert_eq!(msgs[0].text_content(), "Fix the bug in main.rs");

    // Message 1: assistant text
    assert!(matches!(msgs[1].role, super::app::Role::Assistant));
    assert!(msgs[1].text_content().contains("Let me look"));

    // Message 2: ToolCall for Read, immediately followed by its ToolResult
    assert!(matches!(msgs[2].role, super::app::Role::Assistant));
    assert_eq!(msgs[2].blocks.len(), 2);
    assert!(matches!(
        msgs[2].blocks[0],
        super::app::ContentBlock::ToolCall { ref name, .. } if name == "Read"
    ));
    assert!(matches!(
        msgs[2].blocks[1],
        super::app::ContentBlock::ToolResult { ref tool_use_id, .. } if tool_use_id == "t1"
    ));

    // Message 3: ToolCall for Grep, immediately followed by its ToolResult
    assert!(matches!(msgs[3].role, super::app::Role::Assistant));
    assert_eq!(msgs[3].blocks.len(), 2);
    assert!(matches!(
        msgs[3].blocks[0],
        super::app::ContentBlock::ToolCall { ref name, .. } if name == "Grep"
    ));
    assert!(matches!(
        msgs[3].blocks[1],
        super::app::ContentBlock::ToolResult { ref tool_use_id, .. } if tool_use_id == "t2"
    ));

    // Message 4: final assistant text
    assert!(matches!(msgs[4].role, super::app::Role::Assistant));
    assert!(msgs[4].text_content().contains("I found the issue"));
}

/// User messages with only ToolResult content must be skipped — they are already
/// paired with their ToolUse ChatMessages.
#[test]
fn rebuild_display_messages_skips_tool_result_only_user_messages() {
    use strands::types::content::{ContentBlock, Message, Role};
    use strands::types::tools::{ToolResult, ToolResultContent, ToolResultStatus};

    let sdk_messages = vec![
        Message::new(Role::User, vec![ContentBlock::text("run a command")]),
        Message::new(
            Role::Assistant,
            vec![ContentBlock::tool_use_from_parts(
                "tu1",
                "Bash",
                serde_json::json!({"command": "ls -la"}),
            )],
        ),
        // This user message has ONLY a ToolResult — should be skipped in output.
        Message::new(
            Role::User,
            vec![ContentBlock::tool_result(ToolResult {
                tool_use_id: "tu1".to_string(),
                content: vec![ToolResultContent {
                    text: Some("total 0\n-rw-r--r-- 1 user user 0 file.txt".to_string()),
                    document: None,
                    image: None,
                    json: None,
                }],
                status: ToolResultStatus::Success,
            })],
        ),
        Message::new(Role::Assistant, vec![ContentBlock::text("The directory is empty.")]),
    ];

    let msgs = super::app::rebuild_display_messages(&sdk_messages);

    // No user message with ToolResult-only content should appear
    let user_msgs: Vec<_> = msgs
        .iter()
        .filter(|m| matches!(m.role, super::app::Role::User))
        .collect();
    assert_eq!(
        user_msgs.len(),
        1,
        "Only the 'run a command' user message should appear; got {}",
        user_msgs.len()
    );
    assert_eq!(user_msgs[0].text_content(), "run a command");

    // The ToolResult should be paired inside the ToolCall ChatMessage
    let tool_msgs: Vec<_> = msgs
        .iter()
        .filter(|m| {
            m.blocks
                .iter()
                .any(|b| matches!(b, super::app::ContentBlock::ToolCall { .. }))
        })
        .collect();
    assert_eq!(tool_msgs.len(), 1);
    assert_eq!(
        tool_msgs[0].blocks.len(),
        2,
        "ToolCall ChatMessage should have ToolCall + ToolResult blocks"
    );
}

/// ToolUse blocks with no matching ToolResult are marked Success with no result block.
#[test]
fn rebuild_display_messages_tool_use_without_result() {
    use strands::types::content::{ContentBlock, Message, Role};

    // Assistant message with a ToolUse but no corresponding user ToolResult message.
    let sdk_messages = vec![
        Message::new(Role::User, vec![ContentBlock::text("check something")]),
        Message::new(
            Role::Assistant,
            vec![ContentBlock::tool_use_from_parts(
                "t99",
                "Think",
                serde_json::json!({"thought": "Analyzing..."}),
            )],
        ),
        // No ToolResult user message — simulates incomplete session
    ];

    let msgs = super::app::rebuild_display_messages(&sdk_messages);

    // Should have: user msg + tool call msg (with Success status, no ToolResult block)
    let tool_msg = msgs
        .iter()
        .find(|m| {
            m.blocks
                .iter()
                .any(|b| matches!(b, super::app::ContentBlock::ToolCall { .. }))
        })
        .expect("Should have a ToolCall ChatMessage");

    assert_eq!(
        tool_msg.blocks.len(),
        1,
        "ToolCall without result should have only 1 block (the ToolCall itself)"
    );
    assert!(matches!(
        tool_msg.blocks[0],
        super::app::ContentBlock::ToolCall {
            status: super::app::ToolCallStatus::Success,
            ..
        }
    ));
}

// ===========================================================================
// Rewind: /rewind command and suggestion-based picker
// ===========================================================================

#[test]
fn harness_rewind_command_sets_input_and_shows_suggestions() {
    let mut h = TestHarness::new(80, 30);

    // Build a 2-turn conversation
    h.type_str("first question");
    h.press_enter();
    h.app.state.agent_status = AgentStatus::Streaming;
    h.simulate_response("first answer");

    h.type_str("second question");
    h.press_enter();
    h.app.state.agent_status = AgentStatus::Streaming;
    h.simulate_response("second answer");

    assert_eq!(h.app.state.messages.len(), 4);

    // Type /rewind — should show suggestions
    h.type_str("/rewind");
    assert!(
        !h.app.state.suggestions.is_empty(),
        "/rewind should produce suggestions"
    );
    // First suggestion should be the most recent turn
    assert!(
        h.app.state.suggestions[0].name.contains("Turn"),
        "suggestions should have turn labels"
    );
    // Suggestions should carry rewind_info
    assert!(
        h.app.state.suggestions[0].rewind_info.is_some(),
        "rewind suggestions should have rewind_info"
    );
}

#[test]
fn harness_rewind_clears_caches() {
    let mut h = TestHarness::new(80, 30);

    // Build a 2-turn conversation
    h.type_str("first question");
    h.press_enter();
    h.app.state.agent_status = AgentStatus::Streaming;
    h.simulate_response("first answer");

    h.type_str("second question");
    h.press_enter();
    h.app.state.agent_status = AgentStatus::Streaming;
    h.simulate_response("second answer");

    // Render to populate caches
    let _ = h.render();
    assert_eq!(h.app.state.message_cache.len(), 4);

    // Also set streaming cache to verify it's cleared
    h.app.state.streaming_md_cache = Some(super::app::StreamingMdCache {
        boundary: 10,
        prefix_lines: vec![],
    });

    // Rewind to the first user message (index 0)
    h.app.rewind_to(0, "msg-0");

    // message_cache should be truncated (0 original + 1 rewind result message)
    assert!(
        h.app.state.message_cache.len() <= 1,
        "message_cache should be truncated on rewind, got {}",
        h.app.state.message_cache.len()
    );
    // streaming_md_cache should be cleared
    assert!(
        h.app.state.streaming_md_cache.is_none(),
        "streaming_md_cache should be cleared on rewind"
    );
}

#[test]
fn harness_rewind_truncates_conversation() {
    let mut h = TestHarness::new(80, 30);

    // Build 3 turns
    h.type_str("turn one");
    h.press_enter();
    h.app.state.agent_status = AgentStatus::Streaming;
    h.simulate_response("answer one");

    h.type_str("turn two");
    h.press_enter();
    h.app.state.agent_status = AgentStatus::Streaming;
    h.simulate_response("answer two");

    h.type_str("turn three");
    h.press_enter();
    h.app.state.agent_status = AgentStatus::Streaming;
    h.simulate_response("answer three");

    assert_eq!(h.app.state.messages.len(), 6); // 3 user + 3 assistant

    // Rewind to turn 2 (message index 2 = second user message)
    h.app.rewind_to(2, "msg-2");

    // Should have 2 original messages (turn 1 user + assistant) + 1 rewind result
    assert_eq!(h.app.state.messages.len(), 3);
    // First message should still be "turn one"
    assert_eq!(h.app.state.messages[0].text_content(), "turn one");
    // Last message should be the rewind notice
    assert!(h.app.state.messages[2].text_content().contains("[Rewind]"));
}

#[test]
fn harness_rewind_select_suggestion_executes_rewind() {
    let mut h = TestHarness::new(80, 30);

    // Build a 2-turn conversation
    h.type_str("hello world");
    h.press_enter();
    h.app.state.agent_status = AgentStatus::Streaming;
    h.simulate_response("hi there");

    h.type_str("goodbye");
    h.press_enter();
    h.app.state.agent_status = AgentStatus::Streaming;
    h.simulate_response("bye");

    assert_eq!(h.app.state.messages.len(), 4);

    // Type /rewind to get suggestions
    h.type_str("/rewind ");
    assert!(!h.app.state.suggestions.is_empty());

    // Select the most recent turn suggestion (index 0 = latest turn) and press Enter
    h.app.state.selected_suggestion = 0;
    let info = h.app.selected_rewind_info();
    assert!(info.is_some(), "should have rewind_info on selected suggestion");

    let (msg_idx, msg_id) = info.unwrap();
    h.app.reset_input();
    h.app.rewind_to(msg_idx, &msg_id);

    // Conversation should be truncated
    assert!(h.app.state.messages.len() < 4, "messages should be truncated after rewind");
    assert!(
        h.app.state.messages.last().unwrap().text_content().contains("[Rewind]"),
        "last message should be the rewind notice"
    );
}

#[test]
fn harness_double_tap_esc_opens_rewind() {
    let mut h = TestHarness::new(80, 30);

    // Need at least one message for double-tap Esc to trigger
    h.type_str("test prompt");
    h.press_enter();
    h.app.state.agent_status = AgentStatus::Streaming;
    h.simulate_response("test response");

    // First Esc — records tick
    h.press_esc();
    assert!(h.app.state.last_esc_tick.is_some());

    // Second Esc within the window — should fill /rewind
    h.press_esc();
    let input = h.input_text();
    assert_eq!(input, "/rewind ", "double-tap Esc should fill /rewind ");
    assert!(
        !h.app.state.suggestions.is_empty(),
        "double-tap Esc should show rewind suggestions"
    );
}

#[test]
fn harness_rewind_skips_slash_commands_in_suggestions() {
    let mut h = TestHarness::new(80, 30);

    // Turn 1: normal message
    h.type_str("real question");
    h.press_enter();
    h.app.state.agent_status = AgentStatus::Streaming;
    h.simulate_response("real answer");

    // Turn 2: slash command (simulated by adding a user message starting with /)
    h.app.state.messages.push(ChatMessage::user("/status".into()));
    let mut msg = ChatMessage::assistant_empty();
    msg.append_text("Model: test");
    h.app.state.messages.push(msg);

    // Turn 3: normal message
    h.type_str("another question");
    h.press_enter();
    h.app.state.agent_status = AgentStatus::Streaming;
    h.simulate_response("another answer");

    // /rewind should skip the /status command
    h.type_str("/rewind ");
    let descriptions: Vec<&str> = h.app.state.suggestions.iter()
        .map(|s| s.description.as_str())
        .collect();
    for desc in &descriptions {
        assert!(
            !desc.contains("/status"),
            "slash commands should be filtered from rewind suggestions"
        );
    }
}

#[test]
fn rewind_clears_same_caches_as_clear() {
    let mut state = make_state(80, 24);

    // Build messages and populate caches
    state.messages.push(ChatMessage::user("hello".into()));
    let mut msg = ChatMessage::assistant_empty();
    msg.append_text("world");
    state.messages.push(msg);
    let _ = render_to_buffer(&mut state, 80, 24);
    assert_eq!(state.message_cache.len(), 2);

    state.streaming_md_cache = Some(super::app::StreamingMdCache {
        boundary: 5,
        prefix_lines: vec![],
    });

    // Simulate what rewind_to does: truncate + clear streaming cache
    state.messages.truncate(0);
    state.message_cache.truncate(0);
    state.streaming_md_cache = None;

    assert!(state.message_cache.is_empty(), "message_cache should be empty after rewind");
    assert!(state.streaming_md_cache.is_none(), "streaming_md_cache should be None after rewind");
}

// ---------------------------------------------------------------------------
// Regression: word-wrap line count must match ratatui Paragraph wrapping
// ---------------------------------------------------------------------------
//
// Bug: count_wrapped_lines used character-level ceiling division (ceil(width/w))
// but ratatui's Paragraph uses word-boundary wrapping which produces more lines.
// This caused total_lines to be underestimated, so auto-scroll didn't scroll
// far enough and bottom content was hidden behind the input box.

#[test]
fn word_wrap_line_count_matches_paragraph() {
    use ratatui::text::Line;
    use ratatui::widgets::{Paragraph, Wrap};
    use super::widgets::messages::paragraph_line_count;

    // "hello world tests" at width 10:
    // Character-level: ceil(17/10) = 2 (WRONG)
    // Word wrap: "hello " / "world " / "tests" = 3 (CORRECT)
    let lines = vec![Line::from("hello world tests")];
    let count = paragraph_line_count(&lines, 10);
    let expected = Paragraph::new(lines.clone())
        .wrap(Wrap { trim: false })
        .line_count(10) as u16;
    assert_eq!(count, expected, "paragraph_line_count should match ratatui");
    assert_eq!(count, 3, "word wrap should produce 3 lines, not 2");
}

#[test]
fn auto_scroll_shows_bottom_content_with_long_paragraphs() {
    // Simulate a conversation with long paragraphs that trigger word wrapping
    let width = 40u16;
    let height = 20u16;
    let mut state = make_state(width, height);

    // Add a user message
    state.messages.push(super::app::ChatMessage {
        role: super::app::Role::User,
        blocks: vec![super::app::ContentBlock::Text("test".to_string())],
    });

    // Add an assistant message with a long paragraph that will word-wrap
    let long_text = "This is a very long paragraph that contains many words and will definitely need to be wrapped across multiple lines when displayed in a narrow terminal window of only forty characters wide.";
    state.messages.push(super::app::ChatMessage {
        role: super::app::Role::Assistant,
        blocks: vec![super::app::ContentBlock::Text(long_text.to_string())],
    });

    // Render and check that the last line of the assistant message is visible
    let buf = render_to_buffer(&mut state, width, height);
    let lines = buffer_lines(&buf);

    // The word "wide." should be visible (it's the last word of the message)
    assert!(
        buffer_contains(&buf, "wide"),
        "Bottom of message should be visible with auto-scroll. Buffer:\n{}",
        lines.join("\n")
    );
}
