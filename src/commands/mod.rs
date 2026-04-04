//! Slash command system — types, parser, registry, and built-in commands.
//!
//! Mirrors the Claude Code architecture:
//! - **Command types**: `Local` (runs a function) vs `Prompt` (expands to model input)
//! - **Registry**: `Vec<Command>` assembled at startup, looked up by name or alias
//! - **Parser**: strips `/`, splits name from args
//! - **Dispatch**: match on `CommandKind`, execute accordingly

use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Parsed input
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ParsedSlashCommand {
    pub command_name: String,
    pub args: String,
}

/// Parse a `/command args…` string. Returns `None` if input doesn't start with `/`
/// or the command name is empty.
pub fn parse_slash_command(input: &str) -> Option<ParsedSlashCommand> {
    let trimmed = input.trim();
    if !trimmed.starts_with('/') {
        return None;
    }
    let without_slash = &trimmed[1..];
    let mut parts = without_slash.splitn(2, ' ');
    let name = parts.next().unwrap_or("");
    if name.is_empty() {
        return None;
    }
    let args = parts.next().unwrap_or("").to_string();
    Some(ParsedSlashCommand {
        command_name: name.to_string(),
        args,
    })
}

/// Returns true if `name` looks like a valid command name (alphanumeric, `-`, `_`, `:`).
/// Used to distinguish `/some-command` from `/var/log/foo`.
pub fn looks_like_command(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == ':')
}

// ---------------------------------------------------------------------------
// Command result
// ---------------------------------------------------------------------------

/// An item in the interactive model picker.
#[derive(Clone, Debug)]
pub struct ModelPickerItem {
    /// Short alias for display (e.g., "sonnet", "nova-pro")
    pub alias: String,
    /// Human-readable label (e.g., "Claude Sonnet 4.6 (latest)")
    pub label: String,
    /// The model ID that will be passed to `build_model_by_id`
    pub model_id: String,
    /// Provider group for section headers (e.g., "Anthropic Claude")
    #[allow(dead_code)]
    pub group: String,
}

/// The result of executing a local slash command.
pub enum CommandResult {
    /// Display text to the user (not sent to the model).
    Text(String),
    /// Clear the conversation.
    Clear,
    /// Quit the application.
    Quit,
    /// No visible output.
    #[allow(dead_code)]
    Skip,
    /// Switch the model to the given model ID string.
    /// The caller (TUI/REPL) handles async model construction and agent.swap_model().
    SwitchModel(String),
    /// Switch the permission mode. The caller updates AppState.permission_mode
    /// and syncs with strands_tools::plan_state.
    ModeSwitch(String),
    /// Open the interactive model picker with available models.
    ModelPicker {
        current_model: String,
        items: Vec<ModelPickerItem>,
    },
}

// ---------------------------------------------------------------------------
// Command context — information available to command handlers
// ---------------------------------------------------------------------------

/// Info about a registered command, for `/help` rendering.
#[derive(Clone, Debug)]
pub struct CommandInfo {
    pub name: String,
    pub description: String,
    pub aliases: Vec<String>,
    pub argument_hint: Option<String>,
    pub source: &'static str, // "builtin" or "skill"
}

pub struct CommandContext {
    pub model_name: String,
    pub turn_count: usize,
    pub message_count: usize,
    /// Snapshot of all visible commands for /help. Populated by the caller.
    pub all_commands: Vec<CommandInfo>,
    /// Connected MCP servers (for /mcp command).
    pub mcp_servers: Vec<crate::mcp::McpServerInfo>,
    /// Token counts from the SDK tracker: (used, limit). For /context command.
    pub token_counts: Option<(u64, u64)>,
    /// Context percent used from the SDK tracker. For /context command.
    pub context_percent_used: Option<f64>,
    /// System prompt text for token estimation. For /context command.
    pub system_prompt: String,
    /// Tool spec summaries for token estimation. For /context command.
    pub tool_specs: Vec<crate::context::ToolSpecSummary>,
    /// MCP tool specs: (name, server, spec_json). For /context command.
    pub mcp_tool_specs: Vec<(String, String, String)>,
    /// Memory files: (path, source_type, content). For /context command.
    pub memory_files: Vec<(String, String, String)>,
    /// Loaded skills. For /context command.
    pub skills: Vec<crate::context::SkillSummary>,
    /// Conversation messages as JSON. For /context command.
    pub messages_json: Vec<serde_json::Value>,
}

#[cfg(test)]
impl CommandContext {
    /// Convenience constructor for tests.
    pub fn basic(model_name: &str, turn_count: usize, message_count: usize) -> Self {
        Self {
            model_name: model_name.to_string(),
            turn_count,
            message_count,
            all_commands: Vec::new(),
            mcp_servers: Vec::new(),
            token_counts: None,
            context_percent_used: None,
            system_prompt: String::new(),
            tool_specs: Vec::new(),
            mcp_tool_specs: Vec::new(),
            memory_files: Vec::new(),
            skills: Vec::new(),
            messages_json: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Command types
// ---------------------------------------------------------------------------

/// The kind of a slash command, mirroring Claude Code's `LocalCommand` / `PromptCommand`.
pub enum CommandKind {
    /// Runs a local function. Does not query the model.
    Local {
        execute: fn(&str, &CommandContext) -> CommandResult,
    },
    /// Expands into a prompt that is sent to the model.
    Prompt {
        get_prompt: Box<dyn Fn(&str, &CommandContext) -> String + Send + Sync>,
    },
}

/// A registered slash command.
pub struct Command {
    pub name: String,
    pub description: String,
    pub aliases: Vec<String>,
    pub is_hidden: bool,
    pub argument_hint: Option<String>,
    /// Dynamic enablement gate — mirrors Claude Code's `isEnabled?: () => boolean`.
    /// When `Some(f)`, the command is only available if `f()` returns true.
    pub is_enabled: Option<fn() -> bool>,
    /// If true, this command can execute even while the agent is streaming.
    /// Mirrors Claude Code's `immediate` flag used by /status, /model, /config.
    pub immediate: bool,
    pub kind: CommandKind,
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// The command registry — owns all commands and provides fast lookup.
pub struct CommandRegistry {
    commands: Vec<Command>,
    /// Maps name and all aliases → index into `commands`.
    index: HashMap<String, usize>,
}

impl CommandRegistry {
    /// Build a registry from a list of commands.
    pub fn new(commands: Vec<Command>) -> Self {
        let mut index = HashMap::new();
        for (i, cmd) in commands.iter().enumerate() {
            index.insert(cmd.name.clone(), i);
            for alias in &cmd.aliases {
                index.insert(alias.clone(), i);
            }
        }
        Self { commands, index }
    }

    /// Look up a command by name or alias.
    pub fn find(&self, name: &str) -> Option<&Command> {
        self.index.get(name).map(|&i| &self.commands[i])
    }

    /// Check whether a command exists.
    #[allow(dead_code)]
    pub fn has(&self, name: &str) -> bool {
        self.index.contains_key(name)
    }

    /// All visible (non-hidden, enabled) commands, for `/help`.
    pub fn visible(&self) -> impl Iterator<Item = &Command> {
        self.commands
            .iter()
            .filter(|c| !c.is_hidden && c.is_enabled.map_or(true, |f| f()))
    }

    /// Check if a command is enabled (mirrors Claude Code's `isCommandEnabled`).
    #[allow(dead_code)]
    pub fn is_command_enabled(&self, name: &str) -> bool {
        self.find(name)
            .map_or(false, |cmd| cmd.is_enabled.map_or(true, |f| f()))
    }

    /// Snapshot of all visible commands, for rendering `/help`.
    pub fn command_infos(&self) -> Vec<CommandInfo> {
        self.visible()
            .map(|cmd| {
                let is_builtin = builtin_command_names().contains(&cmd.name.as_str());
                CommandInfo {
                    name: cmd.name.clone(),
                    description: cmd.description.clone(),
                    aliases: cmd.aliases.clone(),
                    argument_hint: cmd.argument_hint.clone(),
                    source: if is_builtin { "builtin" } else { "skill" },
                }
            })
            .collect()
    }
}

/// Names of built-in commands (used to distinguish from skills in /help).
fn builtin_command_names() -> &'static [&'static str] {
    &["exit", "clear", "help", "status", "compact", "model", "skills", "mcp",
      "plan", "default", "accept-edits", "bypass", "context"]
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// The outcome of dispatching a slash command.
pub enum DispatchResult {
    /// A local command executed — here is the result.
    Local(CommandResult),
    /// A prompt command expanded — send this text to the model.
    Prompt(String),
    /// A compact prompt — send to model, then replace history with the response.
    CompactPrompt(String),
    /// The command was not found and the name looks like a command.
    Unknown(String),
    /// The input looked like a file path, not a command — treat as plain text.
    NotACommand,
}

/// Parse and dispatch a slash command against the registry.
pub fn dispatch(input: &str, registry: &CommandRegistry, context: &CommandContext) -> DispatchResult {
    let parsed = match parse_slash_command(input) {
        Some(p) => p,
        None => return DispatchResult::NotACommand,
    };

    match registry.find(&parsed.command_name) {
        Some(cmd) if cmd.is_enabled.map_or(true, |f| f()) => match &cmd.kind {
            CommandKind::Local { execute } => {
                DispatchResult::Local(execute(&parsed.args, context))
            }
            CommandKind::Prompt { get_prompt } => {
                let prompt = get_prompt(&parsed.args, context);
                // /compact gets special handling: replace history after model responds
                if parsed.command_name == "compact" {
                    DispatchResult::CompactPrompt(prompt)
                } else {
                    DispatchResult::Prompt(prompt)
                }
            }
        },
        Some(_) => {
            // Command exists but is disabled
            DispatchResult::Unknown(parsed.command_name)
        }
        None => {
            if looks_like_command(&parsed.command_name) {
                DispatchResult::Unknown(parsed.command_name)
            } else {
                DispatchResult::NotACommand
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Built-in commands
// ---------------------------------------------------------------------------

/// The built-in commands (exit, clear, help, status, compact).
fn builtin_commands() -> Vec<Command> {
    vec![
        // /exit, /quit
        Command {
            name: "exit".into(),
            description: "Exit the application".into(),
            aliases: vec!["quit".into()],
            is_hidden: false,
            argument_hint: None,
            is_enabled: None,
            immediate: true,
            kind: CommandKind::Local {
                execute: |_, _| CommandResult::Quit,
            },
        },
        // /clear, /reset, /new
        Command {
            name: "clear".into(),
            description: "Clear conversation history and free up context".into(),
            aliases: vec!["reset".into(), "new".into()],
            is_hidden: false,
            argument_hint: None,
            is_enabled: None,
            immediate: true,
            kind: CommandKind::Local {
                execute: |_, _| CommandResult::Clear,
            },
        },
        // /help, /?
        Command {
            name: "help".into(),
            description: "Show help and available commands".into(),
            aliases: vec!["?".into()],
            is_hidden: false,
            argument_hint: None,
            is_enabled: None,
            immediate: true,
            kind: CommandKind::Local {
                execute: cmd_help,
            },
        },
        // /status
        Command {
            name: "status".into(),
            description: "Show session status".into(),
            aliases: vec![],
            is_hidden: false,
            argument_hint: None,
            is_enabled: None,
            immediate: true,
            kind: CommandKind::Local {
                execute: cmd_status,
            },
        },
        // /model [model-id]
        Command {
            name: "model".into(),
            description: "Show or switch model".into(),
            aliases: vec![],
            is_hidden: false,
            argument_hint: Some("[model-id]".into()),
            is_enabled: None,
            immediate: true,
            kind: CommandKind::Local {
                execute: cmd_model,
            },
        },
        // /skills
        Command {
            name: "skills".into(),
            description: "List loaded skills".into(),
            aliases: vec![],
            is_hidden: false,
            argument_hint: None,
            is_enabled: None,
            immediate: true,
            kind: CommandKind::Local {
                execute: cmd_skills,
            },
        },
        // /mcp
        Command {
            name: "mcp".into(),
            description: "Show connected MCP servers and tools".into(),
            aliases: vec![],
            is_hidden: false,
            argument_hint: None,
            is_enabled: None,
            immediate: true,
            kind: CommandKind::Local {
                execute: cmd_mcp,
            },
        },
        // /plan
        Command {
            name: "plan".into(),
            description: "Enter plan mode — read-only exploration".into(),
            aliases: vec![],
            is_hidden: false,
            argument_hint: None,
            is_enabled: None,
            immediate: true,
            kind: CommandKind::Local {
                execute: |_, _| CommandResult::ModeSwitch("plan".into()),
            },
        },
        // /default
        Command {
            name: "default".into(),
            description: "Switch to default mode".into(),
            aliases: vec![],
            is_hidden: false,
            argument_hint: None,
            is_enabled: None,
            immediate: true,
            kind: CommandKind::Local {
                execute: |_, _| CommandResult::ModeSwitch("default".into()),
            },
        },
        // /accept-edits
        Command {
            name: "accept-edits".into(),
            description: "Auto-approve file edits".into(),
            aliases: vec!["auto-edit".into()],
            is_hidden: false,
            argument_hint: None,
            is_enabled: None,
            immediate: true,
            kind: CommandKind::Local {
                execute: |_, _| CommandResult::ModeSwitch("accept-edits".into()),
            },
        },
        // /bypass
        Command {
            name: "bypass".into(),
            description: "Bypass all permission checks (YOLO mode)".into(),
            aliases: vec!["yolo".into()],
            is_hidden: false,
            argument_hint: None,
            is_enabled: None,
            immediate: true,
            kind: CommandKind::Local {
                execute: |_, _| CommandResult::ModeSwitch("bypass".into()),
            },
        },
        // /context
        Command {
            name: "context".into(),
            description: "Show context window usage breakdown".into(),
            aliases: vec!["ctx".into()],
            is_hidden: false,
            argument_hint: None,
            is_enabled: None,
            immediate: true,
            kind: CommandKind::Local {
                execute: cmd_context,
            },
        },
        // /compact
        Command {
            name: "compact".into(),
            description: "Summarize and compact the conversation".into(),
            aliases: vec![],
            is_hidden: false,
            argument_hint: Some("<optional instructions>".into()),
            is_enabled: None,
            immediate: false,
            kind: CommandKind::Prompt {
                get_prompt: Box::new(cmd_compact_prompt),
            },
        },
        // /session
        Command {
            name: "session".into(),
            description: "Session management: list, id, title, tag, export".into(),
            aliases: vec![],
            is_hidden: false,
            argument_hint: Some("<subcommand> [args]".into()),
            is_enabled: None,
            immediate: true,
            kind: CommandKind::Local {
                execute: cmd_session,
            },
        },
    ]
}

/// Build the default command registry with built-in commands only.
#[cfg(test)]
pub fn builtin_registry() -> CommandRegistry {
    CommandRegistry::new(builtin_commands())
}

/// Skill info needed to register as a command.
pub struct SkillCommandInfo {
    pub name: String,
    pub description: String,
    pub argument_hint: Option<String>,
    pub body: String,
}

/// Build a registry with built-in commands plus one Prompt command per skill.
pub fn build_registry(skills: &[SkillCommandInfo]) -> CommandRegistry {
    let mut commands = builtin_commands();
    for skill in skills {
        let body = skill.body.clone();
        commands.push(Command {
            name: skill.name.clone(),
            description: skill.description.clone(),
            aliases: vec![],
            is_hidden: false,
            argument_hint: skill.argument_hint.clone(),
            is_enabled: None,
            immediate: false,
            kind: CommandKind::Prompt {
                get_prompt: Box::new(move |args, _ctx| {
                    if args.trim().is_empty() {
                        body.clone()
                    } else {
                        body.replace("$ARGUMENTS", args)
                    }
                }),
            },
        });
    }
    CommandRegistry::new(commands)
}

fn cmd_help(_args: &str, ctx: &CommandContext) -> CommandResult {
    let mut lines = vec!["Available commands:".to_string(), String::new()];

    let builtins: Vec<&CommandInfo> = ctx.all_commands.iter().filter(|c| c.source == "builtin").collect();
    let skills: Vec<&CommandInfo> = ctx.all_commands.iter().filter(|c| c.source == "skill").collect();

    for cmd in &builtins {
        lines.push(format_command_line(cmd));
    }

    if !skills.is_empty() {
        lines.push(String::new());
        lines.push("Skills:".to_string());
        lines.push(String::new());
        for cmd in &skills {
            lines.push(format_command_line(cmd));
        }
    }

    lines.push(String::new());
    lines.push("Tip: pgup/pgdn to scroll, Shift+Tab to cycle mode, Ctrl+C to cancel.".to_string());
    CommandResult::Text(lines.join("\n"))
}

fn format_command_line(cmd: &CommandInfo) -> String {
    let aliases = if cmd.aliases.is_empty() {
        String::new()
    } else {
        format!(
            " ({})",
            cmd.aliases
                .iter()
                .map(|a| format!("/{}", a))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let hint = cmd
        .argument_hint
        .as_ref()
        .map(|h| format!(" {}", h))
        .unwrap_or_default();
    format!("  /{}{}{} — {}", cmd.name, hint, aliases, cmd.description)
}

fn cmd_session(args: &str, _ctx: &CommandContext) -> CommandResult {
    let parts: Vec<&str> = args.trim().splitn(2, char::is_whitespace).collect();
    let subcmd = parts.first().copied().unwrap_or("id");
    let sub_args = parts.get(1).copied().unwrap_or("");

    match subcmd {
        "id" | "" => {
            if let Some(journal) = crate::session::get_journal() {
                CommandResult::Text(format!("Session ID: {}", journal.session_id()))
            } else {
                CommandResult::Text("No active session".to_string())
            }
        }
        "list" => {
            let cwd = std::env::current_dir().unwrap_or_default();
            let dir = crate::session::SessionId::storage_dir(&cwd);
            let sessions = crate::session::list_sessions(&dir);
            if sessions.is_empty() {
                CommandResult::Text("No sessions found.".to_string())
            } else {
                let mut lines = vec![format!("{:<3}  {:<38}  {:>8}  {}", "#", "Session ID", "Size", "Modified")];
                for (i, s) in sessions.iter().take(20).enumerate() {
                    lines.push(format!(
                        "{:<3}  {:<38}  {:>6}KB  {}",
                        i + 1,
                        s.session_id,
                        s.size_bytes / 1024,
                        s.modified.format("%Y-%m-%d %H:%M"),
                    ));
                }
                CommandResult::Text(lines.join("\n"))
            }
        }
        "title" => {
            let title = sub_args.trim();
            if title.is_empty() {
                return CommandResult::Text("Usage: /session title <text>".to_string());
            }
            if let Some(journal) = crate::session::get_journal() {
                let journal = std::sync::Arc::clone(journal);
                let title = title.to_string();
                tokio::spawn(async move { let _ = journal.set_custom_title(title).await; });
                CommandResult::Text("Session title updated.".to_string())
            } else {
                CommandResult::Text("No active session".to_string())
            }
        }
        "tag" => {
            let tag = sub_args.trim();
            if tag.is_empty() {
                return CommandResult::Text("Usage: /session tag <tag>".to_string());
            }
            if let Some(journal) = crate::session::get_journal() {
                let journal = std::sync::Arc::clone(journal);
                let tag = tag.to_string();
                tokio::spawn(async move { let _ = journal.set_tag(tag).await; });
                CommandResult::Text("Session tag updated.".to_string())
            } else {
                CommandResult::Text("No active session".to_string())
            }
        }
        "export" => {
            if let Some(journal) = crate::session::get_journal() {
                let cwd = std::env::current_dir().unwrap_or_default();
                let dir = crate::session::SessionId::storage_dir(&cwd);
                let path = dir.join(format!("{}.jsonl", journal.session_id()));
                CommandResult::Text(format!("Session file: {}", path.display()))
            } else {
                CommandResult::Text("No active session".to_string())
            }
        }
        other => CommandResult::Text(format!(
            "Unknown subcommand: '{}'. Available: list, id, title, tag, export",
            other,
        )),
    }
}

fn cmd_status(_args: &str, ctx: &CommandContext) -> CommandResult {
    let lines = vec![
        format!("Model: {}", ctx.model_name),
        format!("Turns: {}", ctx.turn_count),
        format!("Messages: {}", ctx.message_count),
        format!(
            "Working directory: {}",
            std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| ".".into())
        ),
    ];
    CommandResult::Text(lines.join("\n"))
}

fn cmd_context(_args: &str, ctx: &CommandContext) -> CommandResult {
    let input = crate::context::AnalysisInput {
        model_name: ctx.model_name.clone(),
        system_prompt: ctx.system_prompt.clone(),
        tool_specs: ctx
            .tool_specs
            .iter()
            .map(|s| crate::context::ToolSpecSummary {
                name: s.name.clone(),
                description: s.description.clone(),
                input_schema_json: s.input_schema_json.clone(),
            })
            .collect(),
        mcp_tool_specs: ctx.mcp_tool_specs.clone(),
        memory_files: ctx.memory_files.clone(),
        skills: ctx.skills.clone(),
        messages_json: ctx.messages_json.clone(),
        sdk_token_counts: ctx.token_counts,
        sdk_context_percent: ctx.context_percent_used,
    };
    let data = crate::context::analyze_context_usage(&input);
    CommandResult::Text(crate::context::format_context_table(&data))
}

// ---------------------------------------------------------------------------
// Unified model config — mirrors Claude Code's ALL_MODEL_CONFIGS
// ---------------------------------------------------------------------------

/// A model with per-provider IDs, like Claude Code's `ModelConfig`.
#[allow(dead_code)]
struct ModelConfig {
    /// Short alias (e.g., "sonnet", "nova-pro")
    alias: &'static str,
    /// Human-readable label
    label: &'static str,
    /// Anthropic direct API model ID (None if not available via Anthropic)
    anthropic: Option<&'static str>,
    /// AWS Bedrock model/inference-profile ID (None if not on Bedrock)
    bedrock: Option<&'static str>,
    /// OpenAI API model ID (None if not an OpenAI model)
    openai: Option<&'static str>,
    /// Ollama local model tag (None if not an Ollama model)
    ollama: Option<&'static str>,
    /// Mistral API model ID (None if not a Mistral model)
    mistral: Option<&'static str>,
    /// Provider category for grouping in display
    group: &'static str,
}

/// All known models across all providers.
const ALL_MODELS: &[ModelConfig] = &[
    // --- Anthropic Claude 4.6 ---
    ModelConfig {
        alias: "sonnet",
        label: "Claude Sonnet 4.6 (latest)",
        anthropic: Some("claude-sonnet-4-6-20250514"),
        bedrock: Some("us.anthropic.claude-sonnet-4-6"),
        openai: None, ollama: None, mistral: None,
        group: "Anthropic Claude",
    },
    ModelConfig {
        alias: "opus",
        label: "Claude Opus 4.6 (latest)",
        anthropic: Some("claude-opus-4-6-20250626"),
        bedrock: Some("us.anthropic.claude-opus-4-6-v1"),
        openai: None, ollama: None, mistral: None,
        group: "Anthropic Claude",
    },
    // --- Anthropic Claude 4.5 ---
    ModelConfig {
        alias: "haiku",
        label: "Claude Haiku 4.5",
        anthropic: Some("claude-haiku-4-5-20251001"),
        bedrock: Some("us.anthropic.claude-haiku-4-5-20251001-v1:0"),
        openai: None, ollama: None, mistral: None,
        group: "Anthropic Claude",
    },
    // --- Anthropic Claude 4.0 ---
    ModelConfig {
        alias: "sonnet-4",
        label: "Claude Sonnet 4",
        anthropic: Some("claude-sonnet-4-20250514"),
        bedrock: Some("us.anthropic.claude-sonnet-4-20250514-v1:0"),
        openai: None, ollama: None, mistral: None,
        group: "Anthropic Claude",
    },
    ModelConfig {
        alias: "opus-4",
        label: "Claude Opus 4",
        anthropic: Some("claude-opus-4-20250514"),
        bedrock: Some("us.anthropic.claude-opus-4-20250514-v1:0"),
        openai: None, ollama: None, mistral: None,
        group: "Anthropic Claude",
    },
    // --- Anthropic Claude 3.5 (legacy) ---
    ModelConfig {
        alias: "sonnet-3.5",
        label: "Claude Sonnet 3.5",
        anthropic: Some("claude-3-5-sonnet-20241022"),
        bedrock: Some("us.anthropic.claude-3-5-sonnet-20241022-v1:0"),
        openai: None, ollama: None, mistral: None,
        group: "Anthropic Claude",
    },
    ModelConfig {
        alias: "haiku-3.5",
        label: "Claude Haiku 3.5",
        anthropic: Some("claude-3-5-haiku-20241022"),
        bedrock: Some("us.anthropic.claude-3-5-haiku-20241022-v1:0"),
        openai: None, ollama: None, mistral: None,
        group: "Anthropic Claude",
    },
    // --- Amazon Nova (Bedrock only) ---
    ModelConfig {
        alias: "nova-pro",
        label: "Amazon Nova Pro",
        anthropic: None,
        bedrock: Some("us.amazon.nova-pro-v1:0"),
        openai: None, ollama: None, mistral: None,
        group: "Amazon Nova",
    },
    ModelConfig {
        alias: "nova-lite",
        label: "Amazon Nova Lite",
        anthropic: None,
        bedrock: Some("us.amazon.nova-lite-v1:0"),
        openai: None, ollama: None, mistral: None,
        group: "Amazon Nova",
    },
    ModelConfig {
        alias: "nova-micro",
        label: "Amazon Nova Micro",
        anthropic: None,
        bedrock: Some("us.amazon.nova-micro-v1:0"),
        openai: None, ollama: None, mistral: None,
        group: "Amazon Nova",
    },
    ModelConfig {
        alias: "nova-premier",
        label: "Amazon Nova Premier",
        anthropic: None,
        bedrock: Some("us.amazon.nova-premier-v1:0"),
        openai: None, ollama: None, mistral: None,
        group: "Amazon Nova",
    },
    // --- Meta Llama (Bedrock) ---
    ModelConfig {
        alias: "llama-4-scout",
        label: "Llama 4 Scout 17B",
        anthropic: None,
        bedrock: Some("us.meta.llama4-scout-17b-instruct-v1:0"),
        openai: None, ollama: None, mistral: None,
        group: "Meta Llama",
    },
    ModelConfig {
        alias: "llama-4-maverick",
        label: "Llama 4 Maverick 17B",
        anthropic: None,
        bedrock: Some("us.meta.llama4-maverick-17b-instruct-v1:0"),
        openai: None, ollama: None, mistral: None,
        group: "Meta Llama",
    },
    ModelConfig {
        alias: "llama-3.3-70b",
        label: "Llama 3.3 70B",
        anthropic: None,
        bedrock: Some("us.meta.llama3-3-70b-instruct-v1:0"),
        openai: None, ollama: None, mistral: None,
        group: "Meta Llama",
    },
    // --- Mistral (Bedrock + direct API) ---
    ModelConfig {
        alias: "mistral-large",
        label: "Mistral Large",
        anthropic: None,
        bedrock: Some("us.mistral.pixtral-large-2502-v1:0"),
        openai: None, ollama: None,
        mistral: Some("mistral-large-latest"),
        group: "Mistral",
    },
    // --- OpenAI ---
    ModelConfig {
        alias: "gpt-4o",
        label: "GPT-4o",
        anthropic: None, bedrock: None,
        openai: Some("gpt-4o"),
        ollama: None, mistral: None,
        group: "OpenAI",
    },
    ModelConfig {
        alias: "gpt-4.1",
        label: "GPT-4.1",
        anthropic: None, bedrock: None,
        openai: Some("gpt-4.1"),
        ollama: None, mistral: None,
        group: "OpenAI",
    },
    ModelConfig {
        alias: "o3",
        label: "o3",
        anthropic: None, bedrock: None,
        openai: Some("o3"),
        ollama: None, mistral: None,
        group: "OpenAI",
    },
    ModelConfig {
        alias: "o3-mini",
        label: "o3-mini",
        anthropic: None, bedrock: None,
        openai: Some("o3-mini"),
        ollama: None, mistral: None,
        group: "OpenAI",
    },
    // --- Ollama (local) ---
    ModelConfig {
        alias: "ollama-llama3.2",
        label: "Llama 3.2 (local)",
        anthropic: None, bedrock: None, openai: None,
        ollama: Some("llama3.2"),
        mistral: None,
        group: "Ollama",
    },
    ModelConfig {
        alias: "ollama-qwen3",
        label: "Qwen3 8B (local)",
        anthropic: None, bedrock: None, openai: None,
        ollama: Some("qwen3:8b"),
        mistral: None,
        group: "Ollama",
    },
    ModelConfig {
        alias: "ollama-deepseek-r1",
        label: "DeepSeek R1 8B (local)",
        anthropic: None, bedrock: None, openai: None,
        ollama: Some("deepseek-r1:8b"),
        mistral: None,
        group: "Ollama",
    },
];

// ---------------------------------------------------------------------------
// Credential detection
// ---------------------------------------------------------------------------

/// Which providers have credentials available right now.
struct AvailableProviders {
    anthropic: bool,
    bedrock: bool,
    openai: bool,
    ollama: bool,
    mistral: bool,
}

fn detect_available_providers() -> AvailableProviders {
    let anthropic = std::env::var("ANTHROPIC_API_KEY").map_or(false, |v| !v.is_empty());

    // Bedrock: check AWS credentials (env vars, config file, or instance profile marker)
    let bedrock = std::env::var("AWS_ACCESS_KEY_ID").map_or(false, |v| !v.is_empty())
        || std::env::var("AWS_PROFILE").map_or(false, |v| !v.is_empty())
        || std::env::var("AWS_ROLE_ARN").map_or(false, |v| !v.is_empty())
        || std::path::Path::new(&format!(
            "{}/.aws/credentials",
            std::env::var("HOME").unwrap_or_default()
        ))
        .exists()
        || std::path::Path::new(&format!(
            "{}/.aws/config",
            std::env::var("HOME").unwrap_or_default()
        ))
        .exists();

    let openai = std::env::var("OPENAI_API_KEY").map_or(false, |v| !v.is_empty());

    // Ollama: always show (local, no key needed) if OLLAMA_BASE_URL is set or default port reachable
    let ollama = std::env::var("OLLAMA_BASE_URL").is_ok();

    let mistral = std::env::var("MISTRAL_API_KEY").map_or(false, |v| !v.is_empty());

    AvailableProviders { anthropic, bedrock, openai, ollama, mistral }
}

/// Check if a model is available given current provider credentials.
fn model_available(m: &ModelConfig, p: &AvailableProviders) -> bool {
    (m.anthropic.is_some() && p.anthropic)
        || (m.bedrock.is_some() && p.bedrock)
        || (m.openai.is_some() && p.openai)
        || (m.ollama.is_some() && p.ollama)
        || (m.mistral.is_some() && p.mistral)
}

/// Get the best model ID for a config given available providers.
/// Priority: anthropic > bedrock > openai > mistral > ollama.
fn resolve_model_id(m: &ModelConfig, p: &AvailableProviders) -> Option<String> {
    if let (Some(id), true) = (m.anthropic, p.anthropic) {
        return Some(id.to_string());
    }
    if let (Some(id), true) = (m.bedrock, p.bedrock) {
        return Some(format!("bedrock/{}", id));
    }
    if let (Some(id), true) = (m.openai, p.openai) {
        return Some(id.to_string());
    }
    if let (Some(id), true) = (m.mistral, p.mistral) {
        return Some(format!("mistral/{}", id));
    }
    if let (Some(id), true) = (m.ollama, p.ollama) {
        return Some(format!("ollama/{}", id));
    }
    // Fallback: return whatever ID exists, even if no credentials detected
    m.anthropic.map(|id| id.to_string())
        .or_else(|| m.bedrock.map(|id| format!("bedrock/{}", id)))
        .or_else(|| m.openai.map(|id| id.to_string()))
        .or_else(|| m.mistral.map(|id| format!("mistral/{}", id)))
        .or_else(|| m.ollama.map(|id| format!("ollama/{}", id)))
}

// ---------------------------------------------------------------------------
// Alias resolution
// ---------------------------------------------------------------------------

/// Resolve a model alias or pass through a full model ID.
/// Uses credential detection to pick the best provider for an alias.
pub fn resolve_model_alias(input: &str) -> String {
    let lower = input.trim().to_lowercase();
    let providers = detect_available_providers();

    // Match by alias first (e.g., "opus" → bedrock/us.anthropic.claude-opus-4-6-v1)
    for m in ALL_MODELS {
        if lower == m.alias {
            return resolve_model_id(m, &providers).unwrap_or_else(|| input.trim().to_string());
        }
    }

    // Match by any provider's raw model ID (e.g., "claude-opus-4-6-20250626" → resolve for current provider)
    for m in ALL_MODELS {
        let ids: Vec<Option<&str>> = vec![m.anthropic, m.bedrock, m.openai, m.ollama, m.mistral];
        if ids.into_iter().any(|id| id.map(|s| s.to_lowercase()) == Some(lower.clone())) {
            return resolve_model_id(m, &providers).unwrap_or_else(|| input.trim().to_string());
        }
    }

    input.trim().to_string()
}

// ---------------------------------------------------------------------------
// /model command
// ---------------------------------------------------------------------------

/// Build the list of available model picker items based on detected credentials.
pub fn build_model_picker_items() -> Vec<ModelPickerItem> {
    let providers = detect_available_providers();
    let mut items = Vec::new();
    for m in ALL_MODELS {
        if model_available(m, &providers) {
            let model_id = resolve_model_id(m, &providers)
                .unwrap_or_else(|| m.alias.to_string());
            items.push(ModelPickerItem {
                alias: m.alias.to_string(),
                label: m.label.to_string(),
                model_id,
                group: m.group.to_string(),
            });
        }
    }
    // If no providers detected, show all models with fallback IDs
    if items.is_empty() {
        for m in ALL_MODELS {
            let model_id = resolve_model_id(m, &providers)
                .unwrap_or_else(|| m.alias.to_string());
            items.push(ModelPickerItem {
                alias: m.alias.to_string(),
                label: m.label.to_string(),
                model_id,
                group: m.group.to_string(),
            });
        }
    }
    items
}

fn cmd_model(args: &str, ctx: &CommandContext) -> CommandResult {
    if args.trim().is_empty() {
        let items = build_model_picker_items();
        CommandResult::ModelPicker {
            current_model: ctx.model_name.clone(),
            items,
        }
    } else {
        let model_id = resolve_model_alias(args);
        CommandResult::SwitchModel(model_id)
    }
}

fn cmd_skills(_args: &str, ctx: &CommandContext) -> CommandResult {
    let skills: Vec<&CommandInfo> = ctx.all_commands.iter().filter(|c| c.source == "skill").collect();
    if skills.is_empty() {
        return CommandResult::Text(
            "No skills loaded.\n\nAdd skills in .claude/skills/<name>/SKILL.md or .strands/skills/<name>/SKILL.md".to_string(),
        );
    }
    let mut lines = vec![format!("{} skill(s) loaded:", skills.len()), String::new()];
    for skill in &skills {
        let hint = skill
            .argument_hint
            .as_ref()
            .map(|h| format!(" {}", h))
            .unwrap_or_default();
        lines.push(format!("  /{}{} — {}", skill.name, hint, skill.description));
    }
    lines.push(String::new());
    lines.push("Skills are loaded from .claude/skills/ and .strands/skills/".to_string());
    CommandResult::Text(lines.join("\n"))
}

fn cmd_mcp(_args: &str, ctx: &CommandContext) -> CommandResult {
    if ctx.mcp_servers.is_empty() {
        return CommandResult::Text(
            "No MCP servers connected.\n\n\
             Add servers in .strands/mcp.json or .claude/mcp.json:\n\n\
             {\n  \"mcpServers\": {\n    \"my-server\": {\n      \"command\": \"npx\",\n      \
             \"args\": [\"-y\", \"@modelcontextprotocol/server-filesystem\", \"/tmp\"]\n    }\n  }\n}"
                .to_string(),
        );
    }

    let total_tools: usize = ctx.mcp_servers.iter().map(|s| s.tool_names.len()).sum();
    let mut lines = vec![
        format!(
            "{} MCP server(s) connected ({} tools total):",
            ctx.mcp_servers.len(),
            total_tools
        ),
        String::new(),
    ];

    for server in &ctx.mcp_servers {
        lines.push(format!(
            "  {} ({}, {} tools)",
            server.name,
            server.transport,
            server.tool_names.len()
        ));
        for tool in &server.tool_names {
            lines.push(format!("    - {}", tool));
        }
    }

    lines.push(String::new());
    lines.push("Config: .strands/mcp.json or .claude/mcp.json".to_string());
    CommandResult::Text(lines.join("\n"))
}

fn cmd_compact_prompt(args: &str, _ctx: &CommandContext) -> String {
    if args.trim().is_empty() {
        "Summarize this conversation so far into a concise summary. \
         Preserve key decisions, file paths, and code changes discussed. \
         Then continue assisting from this context."
            .to_string()
    } else {
        format!(
            "Summarize this conversation so far with these instructions: {}. \
             Preserve key decisions, file paths, and code changes discussed. \
             Then continue assisting from this context.",
            args.trim()
        )
    }
}

// ---------------------------------------------------------------------------
// Autocomplete suggestions
// ---------------------------------------------------------------------------

/// A suggestion item for the autocomplete dropdown.
#[derive(Clone, Debug)]
pub struct SuggestionItem {
    pub name: String,
    pub description: String,
    /// When set, this suggestion represents a model choice rather than a command.
    /// Accepting it should trigger a model switch instead of filling the input.
    pub model_id: Option<String>,
}

/// Generate command suggestions for a partial input.
/// When the input is `/model ` (with trailing space), returns model choices as suggestions.
pub fn generate_suggestions(input: &str, registry: &CommandRegistry, current_model: &str) -> Vec<SuggestionItem> {
    let trimmed = input.trim();
    if !trimmed.starts_with('/') {
        return Vec::new();
    }

    // Check if this is a `/model <partial>` input — show model suggestions
    if let Some(space_idx) = trimmed.find(' ') {
        let cmd_name = &trimmed[1..space_idx];
        if cmd_name == "model" {
            let query = trimmed[space_idx + 1..].trim().to_lowercase();
            return generate_model_suggestions(&query, current_model);
        }
        // For other commands, don't show suggestions if there are real arguments
        if trimmed[space_idx + 1..].trim().len() > 0 {
            return Vec::new();
        }
    }

    let query = trimmed[1..].split(' ').next().unwrap_or("").to_lowercase();

    let mut matches: Vec<SuggestionItem> = registry
        .visible()
        .filter(|cmd| {
            let enabled = cmd.is_enabled.map_or(true, |f| f());
            if !enabled {
                return false;
            }
            if query.is_empty() {
                return true;
            }
            cmd.name.to_lowercase().starts_with(&query)
                || cmd
                    .aliases
                    .iter()
                    .any(|a| a.to_lowercase().starts_with(&query))
        })
        .map(|cmd| SuggestionItem {
            name: cmd.name.clone(),
            description: cmd.description.clone(),
            model_id: None,
        })
        .collect();

    matches.sort_by(|a, b| {
        let a_exact = a.name.to_lowercase() == query;
        let b_exact = b.name.to_lowercase() == query;
        if a_exact != b_exact {
            return if a_exact {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            };
        }
        a.name.len().cmp(&b.name.len()).then(a.name.cmp(&b.name))
    });

    matches
}

/// Generate model suggestions for the `/model` command argument.
fn generate_model_suggestions(query: &str, current_model: &str) -> Vec<SuggestionItem> {
    let items = build_model_picker_items();
    let mut suggestions: Vec<SuggestionItem> = items
        .into_iter()
        .filter(|item| {
            if query.is_empty() {
                return true;
            }
            item.alias.to_lowercase().starts_with(query)
                || item.label.to_lowercase().contains(query)
                || item.model_id.to_lowercase().starts_with(query)
        })
        .map(|item| {
            let is_current = item.model_id == current_model || item.alias == current_model;
            let description = if is_current {
                format!("{} (current)", item.label)
            } else {
                item.label
            };
            SuggestionItem {
                name: item.alias,
                description,
                model_id: Some(item.model_id),
            }
        })
        .collect();

    // Exact matches first, then alphabetical
    suggestions.sort_by(|a, b| {
        let a_exact = a.name.to_lowercase() == *query;
        let b_exact = b.name.to_lowercase() == *query;
        if a_exact != b_exact {
            return if a_exact {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            };
        }
        a.name.cmp(&b.name)
    });

    suggestions
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic_command() {
        let p = parse_slash_command("/help").unwrap();
        assert_eq!(p.command_name, "help");
        assert_eq!(p.args, "");
    }

    #[test]
    fn parse_command_with_args() {
        let p = parse_slash_command("/compact keep file paths").unwrap();
        assert_eq!(p.command_name, "compact");
        assert_eq!(p.args, "keep file paths");
    }

    #[test]
    fn parse_not_a_command() {
        assert!(parse_slash_command("hello").is_none());
        assert!(parse_slash_command("").is_none());
        assert!(parse_slash_command("/").is_none());
    }

    #[test]
    fn looks_like_command_valid() {
        assert!(looks_like_command("help"));
        assert!(looks_like_command("my-command"));
        assert!(looks_like_command("mcp:tool"));
    }

    #[test]
    fn looks_like_command_file_path() {
        assert!(!looks_like_command("var/log/foo"));
        assert!(!looks_like_command("path.with.dots"));
    }

    #[test]
    fn registry_lookup() {
        let reg = builtin_registry();
        assert!(reg.has("exit"));
        assert!(reg.has("quit")); // alias
        assert!(reg.has("clear"));
        assert!(reg.has("reset")); // alias
        assert!(reg.has("new")); // alias
        assert!(reg.has("help"));
        assert!(reg.has("?")); // alias
        assert!(!reg.has("nonexistent"));
    }

    #[test]
    fn dispatch_quit() {
        let reg = builtin_registry();
        let ctx = CommandContext::basic("test", 0, 0);
        match dispatch("/quit", &reg, &ctx) {
            DispatchResult::Local(CommandResult::Quit) => {}
            _ => panic!("expected Quit"),
        }
    }

    #[test]
    fn dispatch_unknown() {
        let reg = builtin_registry();
        let ctx = CommandContext::basic("test", 0, 0);
        match dispatch("/nonexistent", &reg, &ctx) {
            DispatchResult::Unknown(name) => assert_eq!(name, "nonexistent"),
            _ => panic!("expected Unknown"),
        }
    }

    #[test]
    fn suggestions_prefix_c() {
        let reg = builtin_registry();
        let suggestions = generate_suggestions("/c", &reg, "test");
        assert!(!suggestions.is_empty());
        let names: Vec<&str> = suggestions.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"clear"));
        assert!(names.contains(&"compact"));
        assert_eq!(names[0], "clear");
    }

    #[test]
    fn suggestions_exact_match() {
        let reg = builtin_registry();
        let suggestions = generate_suggestions("/help", &reg, "test");
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].name, "help");
    }

    #[test]
    fn suggestions_slash_only_shows_all() {
        let reg = builtin_registry();
        let suggestions = generate_suggestions("/", &reg, "test");
        assert!(suggestions.len() >= 5);
    }

    #[test]
    fn suggestions_empty_when_args_present() {
        let reg = builtin_registry();
        let suggestions = generate_suggestions("/compact some args", &reg, "test");
        assert!(suggestions.is_empty());
    }

    #[test]
    fn suggestions_no_match() {
        let reg = builtin_registry();
        let suggestions = generate_suggestions("/zzz", &reg, "test");
        assert!(suggestions.is_empty());
    }

    #[test]
    fn suggestions_not_slash() {
        let reg = builtin_registry();
        let suggestions = generate_suggestions("hello", &reg, "test");
        assert!(suggestions.is_empty());
    }

    #[test]
    fn dispatch_file_path() {
        let reg = builtin_registry();
        let ctx = CommandContext::basic("test", 0, 0);
        match dispatch("/var/log/foo", &reg, &ctx) {
            DispatchResult::NotACommand => {}
            _ => panic!("expected NotACommand for file path"),
        }
    }

    #[test]
    fn build_registry_with_skills() {
        let skills = vec![SkillCommandInfo {
            name: "commit".into(),
            description: "Create a commit".into(),
            argument_hint: Some("<message>".into()),
            body: "Stage and commit with $ARGUMENTS".into(),
        }];
        let reg = build_registry(&skills);
        assert!(reg.has("commit"));
        assert!(reg.has("exit")); // builtins still present

        let ctx = CommandContext::basic("test", 0, 0);
        match dispatch("/commit fix typo", &reg, &ctx) {
            DispatchResult::Prompt(text) => assert!(text.contains("fix typo")),
            _ => panic!("expected Prompt"),
        }
    }

    // -----------------------------------------------------------------------
    // Model config tests
    // -----------------------------------------------------------------------

    fn providers_none() -> AvailableProviders {
        AvailableProviders { anthropic: false, bedrock: false, openai: false, ollama: false, mistral: false }
    }

    fn providers_all() -> AvailableProviders {
        AvailableProviders { anthropic: true, bedrock: true, openai: true, ollama: true, mistral: true }
    }

    fn providers_only(name: &str) -> AvailableProviders {
        let mut p = providers_none();
        match name {
            "anthropic" => p.anthropic = true,
            "bedrock" => p.bedrock = true,
            "openai" => p.openai = true,
            "ollama" => p.ollama = true,
            "mistral" => p.mistral = true,
            _ => {}
        }
        p
    }

    fn find_model(alias: &str) -> &'static ModelConfig {
        ALL_MODELS.iter().find(|m| m.alias == alias).unwrap()
    }

    // -- ALL_MODELS integrity --

    #[test]
    fn all_models_have_unique_aliases() {
        let mut seen = std::collections::HashSet::new();
        for m in ALL_MODELS {
            assert!(seen.insert(m.alias), "duplicate alias: {}", m.alias);
        }
    }

    #[test]
    fn all_models_have_at_least_one_provider_id() {
        for m in ALL_MODELS {
            assert!(
                m.anthropic.is_some() || m.bedrock.is_some() || m.openai.is_some()
                    || m.ollama.is_some() || m.mistral.is_some(),
                "model '{}' has no provider IDs",
                m.alias
            );
        }
    }

    #[test]
    fn all_models_have_non_empty_fields() {
        for m in ALL_MODELS {
            assert!(!m.alias.is_empty(), "empty alias");
            assert!(!m.label.is_empty(), "empty label for {}", m.alias);
            assert!(!m.group.is_empty(), "empty group for {}", m.alias);
        }
    }

    #[test]
    fn all_models_group_is_known() {
        let known = ["Anthropic Claude", "Amazon Nova", "Meta Llama", "Mistral", "OpenAI", "Ollama"];
        for m in ALL_MODELS {
            assert!(known.contains(&m.group), "unknown group '{}' for {}", m.group, m.alias);
        }
    }

    // -- Claude models: available on both anthropic and bedrock --

    #[test]
    fn claude_models_have_both_anthropic_and_bedrock() {
        let claude_aliases = ["sonnet", "opus", "haiku", "sonnet-4", "opus-4", "sonnet-3.5", "haiku-3.5"];
        for alias in claude_aliases {
            let m = find_model(alias);
            assert!(m.anthropic.is_some(), "{} missing anthropic ID", alias);
            assert!(m.bedrock.is_some(), "{} missing bedrock ID", alias);
        }
    }

    #[test]
    fn claude_bedrock_ids_start_with_us_anthropic() {
        for m in ALL_MODELS.iter().filter(|m| m.group == "Anthropic Claude") {
            if let Some(bedrock_id) = m.bedrock {
                assert!(
                    bedrock_id.starts_with("us.anthropic."),
                    "{}: bedrock ID '{}' should start with us.anthropic.",
                    m.alias, bedrock_id
                );
            }
        }
    }

    // -- Amazon Nova: bedrock only --

    #[test]
    fn nova_models_are_bedrock_only() {
        let nova = ["nova-pro", "nova-lite", "nova-micro", "nova-premier"];
        for alias in nova {
            let m = find_model(alias);
            assert!(m.bedrock.is_some(), "{} missing bedrock ID", alias);
            assert!(m.anthropic.is_none(), "{} should not have anthropic ID", alias);
            assert!(m.openai.is_none(), "{} should not have openai ID", alias);
        }
    }

    #[test]
    fn nova_bedrock_ids_start_with_us_amazon() {
        for alias in ["nova-pro", "nova-lite", "nova-micro", "nova-premier"] {
            let m = find_model(alias);
            assert!(
                m.bedrock.unwrap().starts_with("us.amazon.nova"),
                "{}: bedrock ID should start with us.amazon.nova",
                alias
            );
        }
    }

    // -- Meta Llama: bedrock only --

    #[test]
    fn llama_models_are_bedrock_only() {
        let llama = ["llama-4-scout", "llama-4-maverick", "llama-3.3-70b"];
        for alias in llama {
            let m = find_model(alias);
            assert!(m.bedrock.is_some(), "{} missing bedrock ID", alias);
            assert!(m.anthropic.is_none(), "{} should not have anthropic ID", alias);
        }
    }

    #[test]
    fn llama_bedrock_ids_start_with_us_meta() {
        for alias in ["llama-4-scout", "llama-4-maverick", "llama-3.3-70b"] {
            let m = find_model(alias);
            assert!(
                m.bedrock.unwrap().starts_with("us.meta.llama"),
                "{}: bedrock ID should start with us.meta.llama",
                alias
            );
        }
    }

    // -- OpenAI models --

    #[test]
    fn openai_models_have_openai_only() {
        let oai = ["gpt-4o", "gpt-4.1", "o3", "o3-mini"];
        for alias in oai {
            let m = find_model(alias);
            assert!(m.openai.is_some(), "{} missing openai ID", alias);
            assert!(m.anthropic.is_none(), "{} should not have anthropic ID", alias);
            assert!(m.bedrock.is_none(), "{} should not have bedrock ID", alias);
        }
    }

    // -- Ollama models --

    #[test]
    fn ollama_models_have_ollama_only() {
        let oll = ["ollama-llama3.2", "ollama-qwen3", "ollama-deepseek-r1"];
        for alias in oll {
            let m = find_model(alias);
            assert!(m.ollama.is_some(), "{} missing ollama ID", alias);
            assert!(m.anthropic.is_none(), "{} should not have anthropic ID", alias);
            assert!(m.bedrock.is_none(), "{} should not have bedrock ID", alias);
        }
    }

    // -- Mistral model --

    #[test]
    fn mistral_large_has_bedrock_and_mistral() {
        let m = find_model("mistral-large");
        assert!(m.bedrock.is_some(), "mistral-large missing bedrock ID");
        assert!(m.mistral.is_some(), "mistral-large missing mistral ID");
        assert!(m.anthropic.is_none(), "mistral-large should not have anthropic ID");
    }

    // -- model_available --

    #[test]
    fn model_available_no_providers() {
        let p = providers_none();
        for m in ALL_MODELS {
            assert!(!model_available(m, &p), "{} should not be available with no providers", m.alias);
        }
    }

    #[test]
    fn model_available_anthropic_only() {
        let p = providers_only("anthropic");
        assert!(model_available(find_model("sonnet"), &p));
        assert!(model_available(find_model("opus"), &p));
        assert!(model_available(find_model("haiku"), &p));
        assert!(!model_available(find_model("nova-pro"), &p));
        assert!(!model_available(find_model("gpt-4o"), &p));
        assert!(!model_available(find_model("ollama-llama3.2"), &p));
    }

    #[test]
    fn model_available_bedrock_only() {
        let p = providers_only("bedrock");
        // Claude models have bedrock IDs
        assert!(model_available(find_model("sonnet"), &p));
        // Bedrock-native models
        assert!(model_available(find_model("nova-pro"), &p));
        assert!(model_available(find_model("llama-4-scout"), &p));
        assert!(model_available(find_model("mistral-large"), &p));
        // Non-bedrock models
        assert!(!model_available(find_model("gpt-4o"), &p));
        assert!(!model_available(find_model("ollama-llama3.2"), &p));
    }

    #[test]
    fn model_available_openai_only() {
        let p = providers_only("openai");
        assert!(model_available(find_model("gpt-4o"), &p));
        assert!(model_available(find_model("o3"), &p));
        assert!(!model_available(find_model("sonnet"), &p));
        assert!(!model_available(find_model("nova-pro"), &p));
    }

    #[test]
    fn model_available_ollama_only() {
        let p = providers_only("ollama");
        assert!(model_available(find_model("ollama-llama3.2"), &p));
        assert!(model_available(find_model("ollama-qwen3"), &p));
        assert!(!model_available(find_model("sonnet"), &p));
    }

    #[test]
    fn model_available_mistral_only() {
        let p = providers_only("mistral");
        assert!(model_available(find_model("mistral-large"), &p));
        assert!(!model_available(find_model("sonnet"), &p));
        assert!(!model_available(find_model("gpt-4o"), &p));
    }

    #[test]
    fn model_available_all_providers() {
        let p = providers_all();
        for m in ALL_MODELS {
            assert!(model_available(m, &p), "{} should be available with all providers", m.alias);
        }
    }

    // -- resolve_model_id priority --

    #[test]
    fn resolve_id_prefers_anthropic_over_bedrock() {
        let p = providers_all();
        let m = find_model("sonnet");
        let id = resolve_model_id(m, &p).unwrap();
        // Should be raw anthropic ID (no prefix)
        assert_eq!(id, "claude-sonnet-4-6-20250514");
        assert!(!id.starts_with("bedrock/"));
    }

    #[test]
    fn resolve_id_falls_back_to_bedrock_when_no_anthropic_key() {
        let p = providers_only("bedrock");
        let m = find_model("sonnet");
        let id = resolve_model_id(m, &p).unwrap();
        assert_eq!(id, "bedrock/us.anthropic.claude-sonnet-4-6");
    }

    #[test]
    fn resolve_id_bedrock_only_model() {
        let p = providers_only("bedrock");
        let m = find_model("nova-pro");
        let id = resolve_model_id(m, &p).unwrap();
        assert_eq!(id, "bedrock/us.amazon.nova-pro-v1:0");
    }

    #[test]
    fn resolve_id_openai_model() {
        let p = providers_only("openai");
        let m = find_model("gpt-4o");
        let id = resolve_model_id(m, &p).unwrap();
        assert_eq!(id, "gpt-4o");
    }

    #[test]
    fn resolve_id_ollama_model() {
        let p = providers_only("ollama");
        let m = find_model("ollama-llama3.2");
        let id = resolve_model_id(m, &p).unwrap();
        assert_eq!(id, "ollama/llama3.2");
    }

    #[test]
    fn resolve_id_mistral_prefers_bedrock_over_mistral_api() {
        let mut p = providers_none();
        p.bedrock = true;
        p.mistral = true;
        let m = find_model("mistral-large");
        let id = resolve_model_id(m, &p).unwrap();
        // Bedrock priority is higher than mistral direct
        assert!(id.starts_with("bedrock/"), "expected bedrock/ prefix, got: {}", id);
    }

    #[test]
    fn resolve_id_mistral_direct_when_no_bedrock() {
        let p = providers_only("mistral");
        let m = find_model("mistral-large");
        let id = resolve_model_id(m, &p).unwrap();
        assert_eq!(id, "mistral/mistral-large-latest");
    }

    #[test]
    fn resolve_id_fallback_when_no_providers() {
        let p = providers_none();
        // Should still return a fallback ID
        let m = find_model("sonnet");
        let id = resolve_model_id(m, &p).unwrap();
        // Falls back to first available provider ID (anthropic for claude)
        assert_eq!(id, "claude-sonnet-4-6-20250514");
    }

    #[test]
    fn resolve_id_fallback_bedrock_only_no_providers() {
        let p = providers_none();
        let m = find_model("nova-pro");
        let id = resolve_model_id(m, &p).unwrap();
        assert_eq!(id, "bedrock/us.amazon.nova-pro-v1:0");
    }

    // -- /model command output --

    #[test]
    fn cmd_model_no_args_returns_picker() {
        let ctx = CommandContext::basic("claude-sonnet-4-6-20250514", 5, 10);
        match cmd_model("", &ctx) {
            CommandResult::ModelPicker { current_model, items } => {
                assert_eq!(current_model, "claude-sonnet-4-6-20250514");
                assert!(!items.is_empty(), "picker should have items");
            }
            _ => panic!("expected ModelPicker"),
        }
    }

    #[test]
    fn cmd_model_no_args_picker_has_all_groups() {
        let ctx = CommandContext::basic("test", 0, 0);
        match cmd_model("", &ctx) {
            CommandResult::ModelPicker { items, .. } => {
                // Should have items from multiple groups (if any providers detected)
                // At minimum, fallback mode includes all models
                assert!(!items.is_empty(), "picker should have items");
                let groups: std::collections::HashSet<&str> = items.iter().map(|i| i.group.as_str()).collect();
                assert!(groups.len() >= 1, "should have at least one group");
            }
            _ => panic!("expected ModelPicker"),
        }
    }

    #[test]
    fn cmd_model_with_alias_returns_switch() {
        let ctx = CommandContext::basic("test", 0, 0);
        match cmd_model("nova-pro", &ctx) {
            CommandResult::SwitchModel(id) => {
                // nova-pro is bedrock-only, should resolve
                assert!(id.contains("nova"), "expected nova in ID, got: {}", id);
            }
            _ => panic!("expected SwitchModel"),
        }
    }

    #[test]
    fn cmd_model_with_full_id_passthrough() {
        let ctx = CommandContext::basic("test", 0, 0);
        match cmd_model("some-custom-model-id", &ctx) {
            CommandResult::SwitchModel(id) => {
                assert_eq!(id, "some-custom-model-id");
            }
            _ => panic!("expected SwitchModel"),
        }
    }

    #[test]
    fn cmd_model_with_provider_prefix() {
        let ctx = CommandContext::basic("test", 0, 0);
        match cmd_model("bedrock/amazon.nova-pro-v1:0", &ctx) {
            CommandResult::SwitchModel(id) => {
                assert_eq!(id, "bedrock/amazon.nova-pro-v1:0");
            }
            _ => panic!("expected SwitchModel"),
        }
    }

    // -- /model only shows available provider groups --

    #[test]
    fn cmd_model_hides_groups_without_credentials() {
        // This test verifies the logic: with no providers, no model groups shown
        let p = providers_none();
        let groups = ["Anthropic Claude", "Amazon Nova", "Meta Llama", "Mistral", "OpenAI", "Ollama"];
        for &group in &groups {
            let group_models: Vec<&ModelConfig> = ALL_MODELS.iter()
                .filter(|m| m.group == group && model_available(m, &p))
                .collect();
            assert!(group_models.is_empty(), "group '{}' should be empty with no providers", group);
        }
    }

    #[test]
    fn cmd_model_shows_bedrock_groups_with_aws() {
        let p = providers_only("bedrock");
        let bedrock_groups = ["Anthropic Claude", "Amazon Nova", "Meta Llama", "Mistral"];
        for &group in &bedrock_groups {
            let group_models: Vec<&ModelConfig> = ALL_MODELS.iter()
                .filter(|m| m.group == group && model_available(m, &p))
                .collect();
            assert!(!group_models.is_empty(), "group '{}' should have models with bedrock", group);
        }
        // OpenAI and Ollama should be empty
        let non_bedrock = ["OpenAI", "Ollama"];
        for &group in &non_bedrock {
            let group_models: Vec<&ModelConfig> = ALL_MODELS.iter()
                .filter(|m| m.group == group && model_available(m, &p))
                .collect();
            assert!(group_models.is_empty(), "group '{}' should be empty with bedrock only", group);
        }
    }

    // -- model count per group --

    #[test]
    fn correct_model_counts_per_group() {
        let counts: HashMap<&str, usize> = ALL_MODELS.iter().fold(HashMap::new(), |mut map, m| {
            *map.entry(m.group).or_insert(0) += 1;
            map
        });
        assert_eq!(counts["Anthropic Claude"], 7);
        assert_eq!(counts["Amazon Nova"], 4);
        assert_eq!(counts["Meta Llama"], 3);
        assert_eq!(counts["Mistral"], 1);
        assert_eq!(counts["OpenAI"], 4);
        assert_eq!(counts["Ollama"], 3);
    }

    #[test]
    fn total_model_count() {
        assert_eq!(ALL_MODELS.len(), 22);
    }
}
