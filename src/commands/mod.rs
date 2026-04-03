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

/// The result of executing a local slash command.
pub enum CommandResult {
    /// Display text to the user (not sent to the model).
    Text(String),
    /// Clear the conversation.
    Clear,
    /// Quit the application.
    Quit,
    /// No visible output.
    Skip,
}

// ---------------------------------------------------------------------------
// Command context — information available to command handlers
// ---------------------------------------------------------------------------

pub struct CommandContext {
    pub model_name: String,
    pub turn_count: usize,
    pub message_count: usize,
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
        get_prompt: fn(&str, &CommandContext) -> String,
    },
}

/// A registered slash command.
pub struct Command {
    pub name: &'static str,
    pub description: &'static str,
    pub aliases: &'static [&'static str],
    pub is_hidden: bool,
    pub argument_hint: Option<&'static str>,
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
            index.insert(cmd.name.to_string(), i);
            for alias in cmd.aliases {
                index.insert(alias.to_string(), i);
            }
        }
        Self { commands, index }
    }

    /// Look up a command by name or alias.
    pub fn find(&self, name: &str) -> Option<&Command> {
        self.index.get(name).map(|&i| &self.commands[i])
    }

    /// Check whether a command exists.
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
    pub fn is_command_enabled(&self, name: &str) -> bool {
        self.find(name)
            .map_or(false, |cmd| cmd.is_enabled.map_or(true, |f| f()))
    }
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
                DispatchResult::Prompt(get_prompt(&parsed.args, context))
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

/// Build the default command registry with all built-in commands.
pub fn builtin_registry() -> CommandRegistry {
    CommandRegistry::new(vec![
        // /exit, /quit
        Command {
            name: "exit",
            description: "Exit the application",
            aliases: &["quit"],
            is_hidden: false,
            argument_hint: None,
            is_enabled: None,
            immediate: true,
            kind: CommandKind::Local {
                execute: |_, _| CommandResult::Quit,
            },
        },
        // /clear, /reset, /new — matches Claude Code's clear aliases
        Command {
            name: "clear",
            description: "Clear conversation history and free up context",
            aliases: &["reset", "new"],
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
            name: "help",
            description: "Show help and available commands",
            aliases: &["?"],
            is_hidden: false,
            argument_hint: None,
            is_enabled: None,
            immediate: true,
            kind: CommandKind::Local {
                execute: cmd_help,
            },
        },
        // /status — immediate so it works while streaming
        Command {
            name: "status",
            description: "Show session status",
            aliases: &[],
            is_hidden: false,
            argument_hint: None,
            is_enabled: None,
            immediate: true,
            kind: CommandKind::Local {
                execute: cmd_status,
            },
        },
        // /compact
        Command {
            name: "compact",
            description: "Summarize and compact the conversation",
            aliases: &[],
            is_hidden: false,
            argument_hint: Some("<optional instructions>"),
            is_enabled: None,
            immediate: false,
            kind: CommandKind::Prompt {
                get_prompt: cmd_compact_prompt,
            },
        },
    ])
}

fn cmd_help(_args: &str, _ctx: &CommandContext) -> CommandResult {
    // Build a fresh registry to enumerate visible commands.
    // This mirrors Claude Code's help which calls getCommands() to get the current list.
    let registry = builtin_registry();
    let mut lines = vec!["Available commands:".to_string(), String::new()];
    for cmd in registry.visible() {
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
            .map(|h| format!(" {}", h))
            .unwrap_or_default();
        lines.push(format!(
            "  /{}{}{} — {}",
            cmd.name, hint, aliases, cmd.description
        ));
    }
    lines.push(String::new());
    lines.push("Tip: pgup/pgdn to scroll, Ctrl+C to cancel a running query.".to_string());
    CommandResult::Text(lines.join("\n"))
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

fn cmd_compact_prompt(args: &str, _ctx: &CommandContext) -> String {
    let instructions = if args.trim().is_empty() {
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
    };
    instructions
}

// ---------------------------------------------------------------------------
// Autocomplete suggestions
// ---------------------------------------------------------------------------

/// A suggestion item for the autocomplete dropdown.
/// Mirrors Claude Code's `SuggestionItem` type.
#[derive(Clone, Debug)]
pub struct SuggestionItem {
    /// The command name (without `/` prefix).
    pub name: String,
    /// Description shown beside the command.
    pub description: String,
}

/// Generate command suggestions for a partial input.
/// Mirrors Claude Code's `generateCommandSuggestions` — prefix match, sorted by name length.
pub fn generate_suggestions(input: &str, registry: &CommandRegistry) -> Vec<SuggestionItem> {
    let trimmed = input.trim();
    if !trimmed.starts_with('/') {
        return Vec::new();
    }

    // Don't show suggestions if there are real arguments after the command
    if let Some(space_idx) = trimmed.find(' ') {
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
            // Match on name or any alias by prefix
            cmd.name.to_lowercase().starts_with(&query)
                || cmd
                    .aliases
                    .iter()
                    .any(|a| a.to_lowercase().starts_with(&query))
        })
        .map(|cmd| SuggestionItem {
            name: cmd.name.to_string(),
            description: cmd.description.to_string(),
        })
        .collect();

    // Sort: exact match first, then prefix matches by name length (shorter = closer match),
    // then alphabetically. Mirrors Claude Code's sort priority in generateCommandSuggestions.
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
        // Among prefix matches, shorter name wins (closer to exact)
        a.name.len().cmp(&b.name.len()).then(a.name.cmp(&b.name))
    });

    matches
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
        assert!(reg.has("new")); // alias — matches Claude Code's /clear aliases
        assert!(reg.has("help"));
        assert!(reg.has("?")); // alias
        assert!(!reg.has("nonexistent"));
    }

    #[test]
    fn dispatch_quit() {
        let reg = builtin_registry();
        let ctx = CommandContext {
            model_name: "test".into(),
            turn_count: 0,
            message_count: 0,
        };
        match dispatch("/quit", &reg, &ctx) {
            DispatchResult::Local(CommandResult::Quit) => {}
            _ => panic!("expected Quit"),
        }
    }

    #[test]
    fn dispatch_unknown() {
        let reg = builtin_registry();
        let ctx = CommandContext {
            model_name: "test".into(),
            turn_count: 0,
            message_count: 0,
        };
        match dispatch("/nonexistent", &reg, &ctx) {
            DispatchResult::Unknown(name) => assert_eq!(name, "nonexistent"),
            _ => panic!("expected Unknown"),
        }
    }

    #[test]
    fn suggestions_prefix_c() {
        let reg = builtin_registry();
        let suggestions = generate_suggestions("/c", &reg);
        assert!(!suggestions.is_empty());
        // /clear and /compact both start with "c"
        let names: Vec<&str> = suggestions.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"clear"));
        assert!(names.contains(&"compact"));
        // Shorter name (clear) should come first
        assert_eq!(names[0], "clear");
    }

    #[test]
    fn suggestions_exact_match() {
        let reg = builtin_registry();
        let suggestions = generate_suggestions("/help", &reg);
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].name, "help");
    }

    #[test]
    fn suggestions_slash_only_shows_all() {
        let reg = builtin_registry();
        let suggestions = generate_suggestions("/", &reg);
        // Should show all visible commands
        assert!(suggestions.len() >= 5); // exit, clear, help, status, compact
    }

    #[test]
    fn suggestions_empty_when_args_present() {
        let reg = builtin_registry();
        let suggestions = generate_suggestions("/compact some args", &reg);
        assert!(suggestions.is_empty());
    }

    #[test]
    fn suggestions_no_match() {
        let reg = builtin_registry();
        let suggestions = generate_suggestions("/zzz", &reg);
        assert!(suggestions.is_empty());
    }

    #[test]
    fn suggestions_not_slash() {
        let reg = builtin_registry();
        let suggestions = generate_suggestions("hello", &reg);
        assert!(suggestions.is_empty());
    }

    #[test]
    fn dispatch_file_path() {
        let reg = builtin_registry();
        let ctx = CommandContext {
            model_name: "test".into(),
            turn_count: 0,
            message_count: 0,
        };
        match dispatch("/var/log/foo", &reg, &ctx) {
            DispatchResult::NotACommand => {}
            _ => panic!("expected NotACommand for file path"),
        }
    }
}
