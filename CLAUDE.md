# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Strands CLI — an interactive coding assistant TUI built in Rust, inspired by Claude Code's terminal UI. Binary name: `strands`, package name: `strands-cli`. Depends on `strands-agents` (SDK) and `strands-tools` (tool implementations) as local path dependencies.

## Build & Test Commands

```bash
cargo build                          # build (debug)
cargo build --release                # build (release), binary at target/release/strands
cargo test                           # run all tests (44 total: 15 command + 26 TUI + 3 other)
cargo test --lib commands::tests     # run only command system tests
cargo test --lib tui::tui_tests      # run only TUI integration tests
cargo run                            # run TUI mode (default)
cargo run -- --no-tui                # run plain REPL mode
cargo run -- --prompt "query"        # one-shot non-interactive mode
cargo run -- -p bedrock              # use Bedrock provider
cargo run -- -m claude-sonnet-4-20250514  # specify model ID
```

Required environment: `ANTHROPIC_API_KEY` for Anthropic provider (default), or AWS credentials for `--provider bedrock`.

## Architecture

### Module Layout

```
src/
  main.rs              CLI args (clap), model/tool construction, entry point dispatch
  commands/
    mod.rs             Slash command system: types, parser, registry, dispatch, autocomplete
  context/
    mod.rs             Context gathering (combines git + user context)
    git.rs             Git status, branch, recent commits
    user_context.rs    CWD, platform, shell detection
  prompt/
    mod.rs             Prompt assembly (combines static + dynamic sections)
    section.rs         PromptSection trait
    builder.rs         PromptBuilder (collects and renders sections)
    static_sections.rs Role, tools, guidelines sections
    dynamic_sections.rs Environment, git status, STRANDS.md sections
  repl.rs              Plain-text REPL (--no-tui), single-turn mode (--prompt)
  session.rs           Session persistence: SessionId, storage paths, listing, resume, cache
  title_generator.rs   AI-powered session title generation via model call
  tui/
    mod.rs             TUI entry point, event loop, key handling, title event dispatch
    app.rs             AppState, TuiApp, ChatMessage, agent stream dispatch, title generation
    event.rs           Unified Event enum (terminal + agent + session title events)
    render.rs          4-zone vertical layout (messages / suggestions / input / status)
    terminal.rs        Raw mode, alt screen, mouse capture, event reader task
    tui_tests.rs       26 E2E tests using ratatui TestBackend
    widgets/
      input_bar.rs     tui-textarea wrapper, history nav, argument hint overlay
      messages.rs      Message history rendering with markdown + tool calls
      suggestions.rs   Autocomplete dropdown (max 6 items, scrolling, selection)
      status_bar.rs    Model name, session title, spinner, turn count, scroll indicator
      tool_call.rs     Per-tool-call styled line, group collapsing
      markdown.rs      pulldown-cmark + syntect syntax-highlighted code blocks
```

### Slash Command System (src/commands/)

Modeled after Claude Code's slash command architecture:

- **Command types**: `CommandKind::Local` (runs a function, returns `CommandResult`) and `CommandKind::Prompt` (expands to a string sent to the model)
- **Registry**: `CommandRegistry` — `Vec<Command>` with `HashMap<String, usize>` index for O(1) lookup by name or alias
- **Parser**: `parse_slash_command()` strips `/`, splits name from args. `looks_like_command()` distinguishes `/clear` from `/var/log/foo`
- **Dispatch**: `dispatch()` → parse → registry lookup → `is_enabled` check → match on `CommandKind` → `DispatchResult` enum
- **Autocomplete**: `generate_suggestions()` prefix-matches visible+enabled commands, sorted by exact match then name length
- **Built-in commands**: `/exit` (`/quit`), `/clear` (`/reset`, `/new`), `/help` (`/?`), `/status`, `/compact <instructions>`, `/resume [id|latest]`, `/session list|id|title|tag|export`, `/rename [name]`, `/rewind` (`/checkpoint`)
- **Suggestion-based pickers**: Commands like `/model`, `/resume`, and `/rewind` use the inline autocomplete dropdown instead of modal overlays. Typing `/rewind ` shows turn suggestions; selecting one executes the rewind. This is the standard pattern for interactive selection — add items with domain-specific fields on `SuggestionItem` (e.g. `model_id`, `session_id`, `rewind_info`) and handle selection in `handle_key()`'s Enter branch.

Key fields on `Command`: `name`, `aliases`, `description`, `is_hidden`, `argument_hint`, `is_enabled: Option<fn() -> bool>`, `immediate: bool` (can run while agent streams).

### TUI Event Loop (src/tui/)

```
Terminal (crossterm) → EventStream → Event enum → mpsc channel
                                                      ↓
main loop: match Event {
  Render     → ratatui::Terminal::draw(render::view)
  Tick       → increment tick_count (drives spinner)
  Key        → handle_key() → autocomplete nav / submit / immediate command
  Paste      → insert chars into textarea
  Agent*     → app.handle_agent_event() → update messages/status
}
```

Agent streaming runs in a `tokio::spawn` task. Events from the SDK (`stream_async()`) are mapped from JSON (`event_type` field) to `Event::Agent*` variants and sent back over the channel.

### Key Integration Points

- **TuiApp::submit()** (`app.rs`): Slash commands dispatched via registry before reaching the agent. Prompt-type commands show the `/command` as user message and send expanded text to the model.
- **try_immediate_command()** (`app.rs`): Fast-path allowing `immediate: true` Local commands while agent is streaming (mirrors Claude Code's `handlePromptSubmit` immediate command fast-path).
- **update_suggestions()** (`app.rs`): Called after every keystroke. Preserves selected item across re-filters. Special-cases `/rewind ` and `/checkpoint ` prefixes to generate rewind suggestions from message history (like `/resume ` generates session suggestions).
- **handle_key()** (`mod.rs`): Routes Tab (accept suggestion), Up/Down (navigate), Esc (dismiss), Enter (accept+execute or submit). On Enter with a selected suggestion, checks `selected_model_id()` → `selected_session_id()` → `selected_rewind_info()` → fallback to `accept_suggestion()`.
- **rewind_to()** (`app.rs`): Truncates conversation + restores files from `file_history` snapshots. Called when user selects a rewind suggestion.
- **Double-tap Esc** (`mod.rs`): When idle with empty input, two Esc presses within 800ms fills `/rewind ` and shows suggestions (mirrors Claude Code's `useDoublePress` → `onShowMessageSelector`).
- **tool_call_summary()** (`repl.rs`): Shared between TUI and REPL — formats tool input as human-readable one-liner.

### Dependencies

| Crate | Purpose |
|-------|---------|
| `strands-agents` (local: `../sdk-rust`) | Agent SDK — `Agent`, `Model`, `AgentTool`, streaming |
| `strands-tools` (local: `../tools-rust`) | Tool implementations — file, shell, grep, glob, think |
| `clap` | CLI argument parsing (derive mode) |
| `ratatui` + `crossterm` | Fullscreen TUI framework + terminal backend |
| `tui-textarea` | Multi-line text input widget |
| `pulldown-cmark` + `syntect` | Markdown rendering + syntax highlighting |
| `tokio` (full) | Async runtime |
| `colored` | ANSI colors for plain REPL output |
| `serde_json` | JSON parsing for agent stream events |

### Tool Set

Built in `main.rs::build_tools()`:
- `Bash` — custom `FunctionTool` with safety guards (blocks destructive commands, redirects to dedicated tools)
- `FileReadTool`, `FileWriteTool`, `FileEditTool`, `GlobTool`, `GrepTool` — from strands-tools
- `ShellTool` (async, background) — from strands-tools
- `ThinkTool` (structured reasoning) — from strands-tools

## Code Conventions

- **Conventional commits**: `feat:`, `fix:`, `refactor:`, `perf:`, etc. Title < 70 chars
- **Tests**: `#[cfg(test)] mod tests` inline in source files. TUI tests use `ratatui::backend::TestBackend` for rendering assertions
- **Error handling**: propagate with `?` using `strands::Result<T>`. Print with `eprintln!` + `colored` in REPL
- **Command registration**: add to `builtin_registry()` in `commands/mod.rs`. Each command is a `Command` struct literal with a function pointer
- **Widget pattern**: each widget is a `render_*()` function taking `&AppState` + `&mut Frame` + `Rect`
- **Agent events**: JSON from `agent.stream_async()` matched on `event_type` string field, mapped to `Event` enum variants
- **Prompt building**: modular via `PromptSection` trait. Static sections (role, tools, guidelines) + dynamic sections (environment, git status, STRANDS.md)

## Adding a New Slash Command

1. Add a `Command` entry in `builtin_registry()` (`src/commands/mod.rs`)
2. Set `kind: CommandKind::Local { execute: your_fn }` or `CommandKind::Prompt { get_prompt: your_fn }`
3. Set `immediate: true` if the command should work while the agent is streaming
4. The command automatically appears in `/help`, autocomplete, and works in both TUI and REPL

## Adding a New Tool

1. Implement `AgentTool` trait (or use `FunctionTool` for simple sync tools)
2. Add to `build_tools()` in `main.rs`
3. The tool name appears in the system prompt automatically via `build_system_prompt()`
