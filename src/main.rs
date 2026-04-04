//! Strands CLI — Interactive TUI for Strands Agents
//!
//! A Claude Code-inspired fullscreen TUI that wires core coding tools (shell,
//! file read/write/edit, glob, grep, think) to a configurable model provider.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use colored::Colorize;
use serde_json::json;

use strands::agent::state::AgentState;
use strands::session::SessionManager as _;
use strands::tools::FunctionTool;
use strands::types::content::Message;
use strands::types::tools::{AgentTool, ToolResult, ToolUse};
use strands::{Agent, Result};

// Tools from strands-tools
use strands_tools::advanced::ThinkTool;
use strands_tools::system::ShellTool;
use strands_tools::utility::skill::{
    SkillCallback, SkillExecutionResult, SkillTool, get_skill,
};
use strands_tools::utility::skill_loader::{load_skills_dir, register_loaded_skill};
use strands_tools::{EnterPlanModeTool, ExitPlanModeTool, FileEditTool, FileReadTool, FileWriteTool, GlobTool, GrepTool};

mod commands;
mod context;
mod mcp;
mod prompt;
mod repl;
pub mod session;
pub mod title_generator;
mod tui;

// ---------------------------------------------------------------------------
// CLI arguments
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(name = "strands", about = "Interactive CLI for Strands Agents")]
struct Cli {
    /// Model provider: anthropic, bedrock, openai, ollama, mistral
    #[arg(short, long, default_value = "anthropic", env = "STRANDS_PROVIDER")]
    provider: String,

    /// Model ID (e.g. "claude-sonnet-4-20250514")
    #[arg(short = 'm', long, env = "STRANDS_MODEL")]
    model: Option<String>,

    /// System prompt override
    #[arg(short, long)]
    system: Option<String>,

    /// Maximum agent iterations per turn (tool-call loops)
    #[arg(long, default_value = "30")]
    max_iterations: usize,

    /// Context window size in tokens (for proactive compaction)
    #[arg(long, default_value = "200000", env = "STRANDS_CONTEXT_WINDOW")]
    context_window: u64,

    /// Maximum tokens per model response
    #[arg(long = "max-tokens", default_value = "16384", env = "STRANDS_MAX_TOKENS")]
    max_tokens: i32,

    /// Run a single prompt (non-interactive, plain output)
    #[arg(long = "prompt")]
    oneshot: Option<String>,

    /// Disable fullscreen TUI, use plain-text REPL instead
    #[arg(long = "no-tui")]
    no_tui: bool,

    /// Resume the most recent session (or a specific one with --session)
    #[arg(long)]
    resume: bool,

    /// Specific session ID to resume (implies --resume)
    #[arg(long)]
    session: Option<String>,

    /// Set a name/title for this session
    #[arg(short = 'n', long = "name")]
    name: Option<String>,
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let model_name = cli
        .model
        .clone()
        .unwrap_or_else(|| match cli.provider.as_str() {
            "bedrock" => "bedrock/default".to_string(),
            _ => "claude-sonnet-4-6-20250514".to_string(),
        });

    // Build model
    let model = build_model(&cli).await?;
    // Keep a clone for background tasks (e.g. AI title generation)
    let model_for_tui = Arc::clone(&model);

    // Build tools (native)
    let mut tools = build_tools();

    // Gather context
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let git_ctx = context::get_git_status(&cwd);
    let user_ctx = context::get_user_context(&cwd);

    // Load MCP servers for non-TUI modes (TUI loads them in background after UI appears)
    let mut _mcp_hold: Option<(Vec<strands::tools::mcp::MCPClient>, Vec<strands::tools::mcp::MCPHttpClient>)> = None;
    let (mcp_server_names, mcp_servers_for_repl) = if cli.no_tui || cli.oneshot.is_some() {
        let session = mcp::load_mcp_servers(&cwd, false).await;
        let names = session.server_names;
        let servers = session.servers;
        tools.extend(session.tools);
        // Keep clients alive until end of main (Drop kills subprocesses)
        _mcp_hold = Some((session.stdio_clients, session.http_clients));
        (names, servers)
    } else {
        (Vec::new(), Vec::new())
    };

    // Load skills from .strands/skills/ and .claude/skills/
    let (skill_infos, skill_cmd_infos) = load_skills(&cwd, &mut tools);

    // Build command registry with skills
    let command_registry = commands::build_registry(&skill_cmd_infos);

    // Build system prompt
    let tool_names: Vec<String> = tools.iter().map(|t| t.tool_name().to_string()).collect();
    let cwd_str = cwd.display().to_string();
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let render_ctx = prompt::RenderContext {
        tool_names: &tool_names,
        cwd: &cwd_str,
        platform: std::env::consts::OS,
        shell: "bash",
        git: git_ctx.as_ref(),
        date: &date,
        has_user_context: user_ctx.is_some(),
        skills: &skill_infos,
        mcp_server_names: &mcp_server_names,
    };
    let source = match cli.system.clone() {
        Some(s) => prompt::PromptSource::Override(s),
        None => prompt::PromptSource::Default,
    };
    let system_prompt = prompt::build_effective_system_prompt(source, &render_ctx);
    let system_prompt_for_ctx = system_prompt.clone();

    // Pre-extract tool specs for /context command (before tools are moved into agent)
    let tool_specs_for_ctx: Vec<context::ToolSpecSummary> = tools
        .iter()
        .map(|t| {
            let spec = t.tool_spec();
            context::ToolSpecSummary {
                name: spec.name.clone(),
                description: spec.description.clone(),
                input_schema_json: serde_json::to_string(&spec.input_schema).unwrap_or_default(),
            }
        })
        .collect();

    // -- Session persistence (JSONL journal) ---------------------------------
    let sessions_dir = session::SessionId::storage_dir(&cwd);

    // Determine session ID: resume an existing session or create a new one
    let resume_ref = cli.session.as_deref().or(if cli.resume { Some("latest") } else { None });
    let (session_id, resumed_messages, mut session_title) = if let Some(session_ref) = resume_ref {
        match session::resolve_and_load_full(&sessions_dir, session_ref).await {
            Ok(resolved) => {
                eprintln!("Resumed session {} ({} messages)", resolved.session_id, resolved.messages.len());
                (session::SessionId::from_existing(resolved.session_id), resolved.messages, resolved.title)
            }
            Err(e) => {
                eprintln!("Warning: could not resume session: {e}");
                (session::SessionId::new(), Vec::new(), None)
            }
        }
    } else {
        (session::SessionId::new(), Vec::new(), None)
    };

    let journal_mgr = strands::session::JournalSessionManager::new(
        session_id.as_str().to_string(),
        Some(sessions_dir),
        Some(30), // retention_days
    )
    .await
    .map_err(|e| strands::Error::Session(format!("journal init: {e}")))?;
    session::set_journal(Arc::clone(&journal_mgr));
    // Keep a clone for registering hooks after agent build (the Arc is moved into the builder)
    let journal_for_hooks = Arc::clone(&journal_mgr);

    // --name flag: set a custom session title (overrides any resumed title)
    if let Some(ref name) = cli.name {
        session_title = Some(name.clone());
        let journal = Arc::clone(&journal_for_hooks);
        let title = name.clone();
        tokio::spawn(async move {
            let _ = journal.set_custom_title(title).await;
        });
    }

    // Build agent with proactive context management
    let summarizing = Arc::new(
        strands::agent::conversation_manager::SummarizingConversationManager::new(
            Some(0.3),  // summary_ratio
            Some(10),   // preserve_recent_messages
            None,       // no custom summarization agent
            None,       // no custom summarization prompt
        )?
    );
    let mut builder = Agent::builder()
        .with_model(model)
        .with_system_prompt(system_prompt)
        .with_tools(tools)
        .with_max_iterations(cli.max_iterations)
        .with_conversation_manager(summarizing)
        .with_proactive_context_management(cli.context_window, cli.max_tokens.max(0) as u64)
        .with_session_manager(journal_mgr as Arc<dyn strands::session::SessionManager>)
        .with_agent_id(session_id.as_str().to_string());

    // Inject initial conversation state (resumed messages or STRANDS.md context)
    if !resumed_messages.is_empty() {
        let mut state = AgentState::new();
        for msg in resumed_messages {
            state.add_message(msg);
        }
        builder = builder.with_conversation_state(state);
    } else if let Some(ref user_ctx) = user_ctx {
        let mut state = AgentState::new();
        state.add_message(Message::user(format!(
            "<context>\n{}\n</context>",
            user_ctx.content
        )));
        state.add_message(Message::assistant(
            "I've read the project context. Ready to help.",
        ));
        builder = builder.with_conversation_state(state);
    }

    let agent = builder.build().await?;

    // Register journal session hooks so every message is persisted to disk.
    // The SDK's `session_manager.register_hooks()` is a no-op for JournalSessionManager;
    // the real hooks must be registered via `register_journal_hooks` or manually.
    // We use agent.add_hook() since the agent's HookRegistry is already built.
    {
        let mgr = Arc::clone(&journal_for_hooks);
        agent.add_hook(move |event: &strands::hooks::MessageAddedEvent| {
            let mgr2 = Arc::clone(&mgr);
            let message = event.message.clone();
            let agent_id = event.agent_id.clone();
            tokio::spawn(async move {
                if let Err(e) = mgr2.append_message(message, &agent_id).await {
                    eprintln!("Journal: failed to append message: {e}");
                }
            });
        });
    }
    {
        let mgr = Arc::clone(&journal_for_hooks);
        agent.add_hook(move |event: &strands::hooks::AfterInvocationEvent| {
            let mgr2 = Arc::clone(&mgr);
            let agent_id = event.agent_id.clone();
            tokio::spawn(async move {
                if let Err(e) = mgr2.sync_agent(serde_json::json!({}), &agent_id).await {
                    eprintln!("Journal: failed to sync agent: {e}");
                }
            });
        });
    }

    // Dispatch
    if let Some(prompt) = &cli.oneshot {
        repl::run_single_turn(&agent, prompt).await?;
    } else if cli.no_tui {
        repl::run_repl(&agent, command_registry, mcp_servers_for_repl).await?;
    } else {
        // Build context setup for /context command
        let home_strands = std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join(".strands"));
        let memory_file_data: Vec<(String, String, String)> = if let Some(ref uc) = user_ctx {
            uc.sources
                .iter()
                .filter_map(|p| {
                    let content = std::fs::read_to_string(p).ok()?;
                    let source_type = if home_strands
                        .as_ref()
                        .map_or(false, |hs| p.starts_with(hs))
                    {
                        "global"
                    } else {
                        "project"
                    };
                    Some((p.display().to_string(), source_type.to_string(), content))
                })
                .collect()
        } else {
            Vec::new()
        };
        let skill_data: Vec<context::SkillSummary> = skill_cmd_infos
            .iter()
            .map(|s| context::SkillSummary {
                name: s.name.clone(),
                description: s.description.clone(),
                content: s.body.clone(),
                source: "project".to_string(),
            })
            .collect();
        let ctx_setup = tui::ContextSetup {
            system_prompt: system_prompt_for_ctx,
            tool_specs: tool_specs_for_ctx,
            memory_files: memory_file_data,
            skills: skill_data,
        };
        tui::run(agent, model_name, command_registry, cwd, ctx_setup, Some(session_id.as_str().to_string()), session_title, model_for_tui).await?;
    }

    // Re-append metadata and flush session journal before exit
    if let Some(journal) = session::get_journal() {
        let _ = journal.reappend_metadata().await;
        let _ = journal.flush().await;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Model construction
// ---------------------------------------------------------------------------

async fn build_model(cli: &Cli) -> Result<Arc<dyn strands::types::models::Model>> {
    let max_tokens = cli.max_tokens;
    match cli.provider.as_str() {
        "anthropic" => {
            use strands::models::anthropic::{AnthropicConfig, AnthropicModel};

            let model_id = cli
                .model
                .clone()
                .unwrap_or_else(|| "claude-sonnet-4-6-20250514".to_string());

            let config = AnthropicConfig {
                model_id: model_id.clone(),
                max_tokens: Some(max_tokens),
                ..Default::default()
            };

            let api_key = std::env::var("ANTHROPIC_API_KEY").ok();
            let model = AnthropicModel::new(Some(model_id), api_key, None, config).await?;

            Ok(Arc::new(model))
        }
        "bedrock" => {
            use strands::models::bedrock::{BedrockConfig, BedrockModel};

            let model_id = cli.model.clone()
                .unwrap_or_else(|| "us.anthropic.claude-sonnet-4-6".to_string());
            let mut config = BedrockConfig::default();
            config.model_id = model_id;
            config.max_tokens = Some(max_tokens);

            let model = BedrockModel::new(None, None, Some("us-east-1".to_string()), config).await?;

            Ok(Arc::new(model))
        }
        "openai" => {
            use strands::models::openai::{OpenAIConfig, OpenAIModel};

            let model_id = cli
                .model
                .clone()
                .unwrap_or_else(|| "gpt-4o".to_string());

            let api_key = std::env::var("OPENAI_API_KEY").ok();
            let config = OpenAIConfig {
                model_id: model_id.clone(),
                max_tokens: Some(max_tokens),
                ..Default::default()
            };

            let model = OpenAIModel::new(Some(model_id), api_key, None, Some(config)).await?;
            Ok(Arc::new(model))
        }
        "ollama" => {
            use strands::models::ollama::{OllamaConfig, OllamaModel};

            let model_id = cli.model.clone().unwrap_or_else(|| "llama3.2".to_string());
            let base_url = std::env::var("OLLAMA_BASE_URL").ok();
            let config = OllamaConfig {
                model_id: model_id.clone(),
                ..Default::default()
            };

            let model = OllamaModel::new(Some(model_id), base_url, config).await?;
            Ok(Arc::new(model))
        }
        "mistral" => {
            use strands::models::mistral::{MistralConfig, MistralModel};

            let model_id = cli
                .model
                .clone()
                .unwrap_or_else(|| "mistral-large-latest".to_string());

            let api_key = std::env::var("MISTRAL_API_KEY").ok();
            let config = MistralConfig {
                model_id: model_id.clone(),
                ..Default::default()
            };

            let model = MistralModel::new(Some(model_id), api_key, None, config).await?;
            Ok(Arc::new(model))
        }
        other => {
            eprintln!(
                "{} Unknown provider '{}'. Supported: anthropic, bedrock, openai, ollama, mistral.",
                "error:".red().bold(),
                other
            );
            std::process::exit(1);
        }
    }
}

/// Build a model by ID string — used for runtime `/model` switching.
/// Detects provider from model ID prefix, explicit `provider/model` syntax, or environment.
///
/// Provider detection rules:
/// - `bedrock/MODEL` or Bedrock-style IDs (`global.*`, `us.*`, ARNs with `:`) → Bedrock
/// - `openai/MODEL` or OpenAI-style IDs (`gpt-*`, `o1-*`, `o3-*`) → OpenAI
/// - `ollama/MODEL` → Ollama (local)
/// - `mistral/MODEL` or `mistral-*` → Mistral
/// - `claude-*` or unrecognized → Anthropic direct API (default)
pub async fn build_model_by_id(model_id: &str) -> Result<Arc<dyn strands::types::models::Model>> {
    // Check for explicit provider/ prefix
    if let Some(rest) = model_id.strip_prefix("bedrock/") {
        return build_bedrock_model(rest).await;
    }
    if let Some(rest) = model_id.strip_prefix("openai/") {
        return build_openai_model(rest).await;
    }
    if let Some(rest) = model_id.strip_prefix("ollama/") {
        return build_ollama_model(rest).await;
    }
    if let Some(rest) = model_id.strip_prefix("mistral/") {
        return build_mistral_model(rest).await;
    }
    if let Some(rest) = model_id.strip_prefix("anthropic/") {
        return build_anthropic_model(rest).await;
    }

    // Auto-detect from model ID pattern
    let is_bedrock = model_id.starts_with("us.")
        || model_id.starts_with("eu.")
        || model_id.starts_with("ap.")
        || model_id.starts_with("global.")
        || model_id.starts_with("amazon.")
        || model_id.starts_with("meta.")
        || model_id.contains(":"); // Bedrock ARN-style IDs

    if is_bedrock {
        return build_bedrock_model(model_id).await;
    }

    if model_id.starts_with("gpt-") || model_id.starts_with("o1-") || model_id.starts_with("o3-") {
        return build_openai_model(model_id).await;
    }

    if model_id.starts_with("mistral-") || model_id.starts_with("codestral-") || model_id.starts_with("pixtral-") {
        return build_mistral_model(model_id).await;
    }

    // Default: for Claude models, prefer Anthropic direct API if a key is
    // available; otherwise fall back to Bedrock (the user likely started with
    // --provider bedrock and AWS credentials).
    if model_id.starts_with("claude-") && std::env::var("ANTHROPIC_API_KEY").is_err() {
        return build_bedrock_model(model_id).await;
    }

    // Default: Anthropic direct API
    build_anthropic_model(model_id).await
}

/// Read max_tokens from env or use default (for runtime model switching).
fn runtime_max_tokens() -> i32 {
    std::env::var("STRANDS_MAX_TOKENS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(16384)
}

async fn build_anthropic_model(model_id: &str) -> Result<Arc<dyn strands::types::models::Model>> {
    use strands::models::anthropic::{AnthropicConfig, AnthropicModel};
    let config = AnthropicConfig {
        model_id: model_id.to_string(),
        max_tokens: Some(runtime_max_tokens()),
        ..Default::default()
    };
    let api_key = std::env::var("ANTHROPIC_API_KEY").ok();
    let model = AnthropicModel::new(Some(model_id.to_string()), api_key, None, config).await?;
    Ok(Arc::new(model))
}

async fn build_bedrock_model(model_id: &str) -> Result<Arc<dyn strands::types::models::Model>> {
    use strands::models::bedrock::{BedrockConfig, BedrockModel};
    let mut config = BedrockConfig::default();
    config.model_id = model_id.to_string();
    config.max_tokens = Some(runtime_max_tokens());
    let model = BedrockModel::new(None, None, Some("us-east-1".to_string()), config).await?;
    Ok(Arc::new(model))
}

async fn build_openai_model(model_id: &str) -> Result<Arc<dyn strands::types::models::Model>> {
    use strands::models::openai::{OpenAIConfig, OpenAIModel};
    let api_key = std::env::var("OPENAI_API_KEY").ok();
    let config = OpenAIConfig {
        model_id: model_id.to_string(),
        max_tokens: Some(runtime_max_tokens()),
        ..Default::default()
    };
    let model = OpenAIModel::new(Some(model_id.to_string()), api_key, None, Some(config)).await?;
    Ok(Arc::new(model))
}

async fn build_ollama_model(model_id: &str) -> Result<Arc<dyn strands::types::models::Model>> {
    use strands::models::ollama::{OllamaConfig, OllamaModel};
    let base_url = std::env::var("OLLAMA_BASE_URL").ok();
    let config = OllamaConfig {
        model_id: model_id.to_string(),
        ..Default::default()
    };
    let model = OllamaModel::new(Some(model_id.to_string()), base_url, config).await?;
    Ok(Arc::new(model))
}

async fn build_mistral_model(model_id: &str) -> Result<Arc<dyn strands::types::models::Model>> {
    use strands::models::mistral::{MistralConfig, MistralModel};
    let api_key = std::env::var("MISTRAL_API_KEY").ok();
    let config = MistralConfig {
        model_id: model_id.to_string(),
        ..Default::default()
    };
    let model = MistralModel::new(Some(model_id.to_string()), api_key, None, config).await?;
    Ok(Arc::new(model))
}

// ---------------------------------------------------------------------------
// Tool construction
// ---------------------------------------------------------------------------

fn build_tools() -> Vec<Arc<dyn AgentTool>> {
    let mut tools: Vec<Arc<dyn AgentTool>> = Vec::new();

    // Bash (FunctionTool — sync shell execution with safety guards)
    let bash_schema = json!({
        "type": "object",
        "properties": {
            "command": {
                "type": "string",
                "description": "The bash command to execute"
            },
            "timeout": {
                "type": "integer",
                "description": "Timeout in seconds (default 120, max 600)"
            }
        },
        "required": ["command"]
    })
    .as_object()
    .unwrap()
    .iter()
    .map(|(k, v)| (k.clone(), v.clone()))
    .collect();

    tools.push(Arc::new(FunctionTool::new(
        "Bash",
        "Execute a bash command. Use dedicated tools (Read, Edit, Glob, Grep) instead of shell equivalents (cat, sed, find, grep).",
        bash_schema,
        bash_execute,
    )));

    // File tools from strands-tools
    tools.push(Arc::new(FileReadTool::new()));
    tools.push(Arc::new(FileWriteTool::new()));
    tools.push(Arc::new(FileEditTool::new()));
    tools.push(Arc::new(GlobTool::new()));
    tools.push(Arc::new(GrepTool::new()));

    // Shell tool (async, background support)
    tools.push(Arc::new(ShellTool::new()));

    // Think tool (structured reasoning)
    tools.push(Arc::new(ThinkTool::new()));

    // Plan mode tools
    tools.push(Arc::new(EnterPlanModeTool::new()));
    tools.push(Arc::new(ExitPlanModeTool::new()));

    tools
}

fn bash_execute(tool_use: &ToolUse) -> Result<ToolResult> {
    let command = tool_use
        .input
        .get("command")
        .and_then(|v| v.as_str())
        .ok_or_else(|| strands::Error::ToolExecution("Missing 'command' parameter".into()))?;

    let _timeout_secs = tool_use
        .input
        .get("timeout")
        .and_then(|v| v.as_u64())
        .unwrap_or(120)
        .min(600);

    // Block dangerous commands
    let blocked = ["rm -rf /", "mkfs", "dd if=/dev/zero", "> /dev/sd"];
    if blocked.iter().any(|p| command.contains(p)) {
        return Ok(ToolResult::error(
            tool_use.tool_use_id.clone(),
            "Blocked: potentially destructive command",
        ));
    }

    // Redirect to dedicated tools
    let redirects: &[(&[&str], &str)] = &[
        (
            &["grep ", "rg "],
            "Use the Grep tool instead of grep/rg via Bash.",
        ),
        (
            &["cat ", "head ", "tail "],
            "Use the Read tool instead of cat/head/tail via Bash.",
        ),
        (&["find "], "Use the Glob tool instead of find via Bash."),
        (
            &["sed ", "awk "],
            "Use the Edit tool instead of sed/awk via Bash.",
        ),
    ];
    for (patterns, msg) in redirects {
        if patterns.iter().any(|p| command.starts_with(p)) {
            return Ok(ToolResult::error(
                tool_use.tool_use_id.clone(),
                msg.to_string(),
            ));
        }
    }

    match std::process::Command::new("bash")
        .arg("-c")
        .arg(command)
        .env("TERM", "dumb")
        .output()
    {
        Ok(output) => {
            let mut result = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !stderr.is_empty() {
                if !result.is_empty() {
                    result.push('\n');
                }
                result.push_str("stderr:\n");
                result.push_str(&stderr);
            }
            if !output.status.success() {
                result.push_str(&format!(
                    "\nExit code: {}",
                    output.status.code().unwrap_or(-1)
                ));
            }
            if result.len() > 30_000 {
                result.truncate(30_000);
                result.push_str("\n... (output truncated at 30KB)");
            }
            if result.is_empty() {
                result = "(no output)".into();
            }
            Ok(ToolResult::success(tool_use.tool_use_id.clone(), result))
        }
        Err(e) => Ok(ToolResult::error(
            tool_use.tool_use_id.clone(),
            format!("Failed to execute command: {}", e),
        )),
    }
}

// ---------------------------------------------------------------------------
// Skill loading
// ---------------------------------------------------------------------------

/// Discover skills, register them, create a SkillTool, and return info for
/// prompt rendering and command registry.
fn load_skills(
    cwd: &std::path::Path,
    tools: &mut Vec<Arc<dyn AgentTool>>,
) -> (Vec<prompt::section::SkillInfo>, Vec<commands::SkillCommandInfo>) {
    use strands_tools::utility::skill_loader::LoadedSkill;

    // Discover skills from .strands/skills/ and .claude/skills/ (project + user home)
    let mut all_skills: Vec<LoadedSkill> = Vec::new();
    for dir_name in &[".strands", ".claude"] {
        let skills_dir = cwd.join(dir_name).join("skills");
        all_skills.extend(load_skills_dir(&skills_dir, "project"));
    }
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        for dir_name in &[".strands", ".claude"] {
            let skills_dir = home.join(dir_name).join("skills");
            all_skills.extend(load_skills_dir(&skills_dir, "user"));
        }
    }

    // Deduplicate by name (later entries win)
    let mut seen: HashMap<String, &LoadedSkill> = HashMap::new();
    for skill in &all_skills {
        seen.insert(skill.name.clone(), skill);
    }

    // Register in global registry and build content map
    let mut skill_content_map: HashMap<String, String> = HashMap::new();
    let mut skill_infos: Vec<prompt::section::SkillInfo> = Vec::new();
    let mut skill_cmd_infos: Vec<commands::SkillCommandInfo> = Vec::new();

    for skill in seen.values() {
        let def = register_loaded_skill(skill);
        skill_content_map.insert(skill.name.clone(), skill.content.clone());

        skill_infos.push(prompt::section::SkillInfo {
            name: def.name.clone(),
            description: def.description.clone(),
            when_to_use: skill.frontmatter.when_to_use.clone(),
        });

        skill_cmd_infos.push(commands::SkillCommandInfo {
            name: skill.name.clone(),
            description: def.description.clone(),
            argument_hint: skill.frontmatter.argument_hint.clone(),
            body: skill.content.clone(),
        });
    }

    // Create SkillTool with callback that returns skill content
    let content_map = skill_content_map;
    let skill_callback: SkillCallback = Arc::new(move |name, args| {
        let _def = get_skill(name)
            .ok_or_else(|| format!("Unknown skill: {}", name))?;
        let content = content_map.get(name).cloned().unwrap_or_default();
        let final_content = match args {
            Some(a) if !a.is_empty() => content.replace("$ARGUMENTS", a),
            _ => content,
        };
        Ok(SkillExecutionResult {
            content: Some(final_content),
            result: None,
            agent_id: None,
        })
    });
    tools.push(Arc::new(SkillTool::with_callback(skill_callback)));

    (skill_infos, skill_cmd_infos)
}

