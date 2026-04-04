//! Strands CLI — Interactive TUI for Strands Agents
//!
//! A Claude Code-inspired fullscreen TUI that wires core coding tools (shell,
//! file read/write/edit, glob, grep, think) to a configurable model provider.

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use colored::Colorize;
use serde_json::json;

use strands::agent::state::AgentState;
use strands::tools::FunctionTool;
use strands::types::content::Message;
use strands::types::tools::{AgentTool, ToolResult, ToolUse};
use strands::{Agent, Result};

// Tools from strands-tools
use strands_tools::advanced::ThinkTool;
use strands_tools::system::ShellTool;
use strands_tools::{FileEditTool, FileReadTool, FileWriteTool, GlobTool, GrepTool};

mod commands;
mod context;
mod mcp;
mod prompt;
mod repl;
mod tui;

// ---------------------------------------------------------------------------
// CLI arguments
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(name = "strands", about = "Interactive CLI for Strands Agents")]
struct Cli {
    /// Model provider: "anthropic" or "bedrock"
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

    /// Run a single prompt (non-interactive, plain output)
    #[arg(long = "prompt")]
    oneshot: Option<String>,

    /// Disable fullscreen TUI, use plain-text REPL instead
    #[arg(long = "no-tui")]
    no_tui: bool,
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
            _ => "claude-sonnet-4-20250514".to_string(),
        });

    // Build model
    let model = build_model(&cli).await?;

    // Build tools (native)
    let mut tools = build_tools();

    // Gather context
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let git_ctx = context::get_git_status(&cwd);
    let user_ctx = context::get_user_context(&cwd);

    // Load MCP servers — async; clients must stay alive for session duration
    let mcp_session = mcp::load_mcp_servers(&cwd).await;
    tools.extend(mcp_session.tools);

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
        mcp_server_names: &mcp_session.server_names,
    };
    let source = match cli.system.clone() {
        Some(s) => prompt::PromptSource::Override(s),
        None => prompt::PromptSource::Default,
    };
    let system_prompt = prompt::build_effective_system_prompt(source, &render_ctx);

    // Build agent
    let mut builder = Agent::builder()
        .with_model(model)
        .with_system_prompt(system_prompt)
        .with_tools(tools)
        .with_max_iterations(cli.max_iterations)
        .with_sliding_window(500);

    // Inject STRANDS.md as initial conversation context
    if let Some(ref user_ctx) = user_ctx {
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

    // Dispatch
    if let Some(prompt) = &cli.oneshot {
        repl::run_single_turn(&agent, prompt).await?;
    } else if cli.no_tui {
        repl::run_repl(&agent).await?;
    } else {
        tui::run(agent, model_name).await?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Model construction
// ---------------------------------------------------------------------------

async fn build_model(cli: &Cli) -> Result<Arc<dyn strands::types::models::Model>> {
    match cli.provider.as_str() {
        "anthropic" => {
            use strands::models::anthropic::{AnthropicConfig, AnthropicModel};

            let model_id = cli
                .model
                .clone()
                .unwrap_or_else(|| "claude-sonnet-4-20250514".to_string());

            let config = AnthropicConfig {
                model_id: model_id.clone(),
                max_tokens: Some(16384),
                ..Default::default()
            };

            let api_key = std::env::var("ANTHROPIC_API_KEY").ok();
            let model = AnthropicModel::new(Some(model_id), api_key, None, config).await?;

            Ok(Arc::new(model))
        }
        "bedrock" => {
            use strands::models::bedrock::{BedrockConfig, BedrockModel};

            let mut config = BedrockConfig::default();
            if let Some(ref model_id) = cli.model {
                config.model_id = model_id.clone();
            }
            config.max_tokens = Some(16384);

            let model = BedrockModel::new(None, None, None, config).await?;

            Ok(Arc::new(model))
        }
        other => {
            eprintln!(
                "{} Unknown provider '{}'. Use 'anthropic' or 'bedrock'.",
                "error:".red().bold(),
                other
            );
            std::process::exit(1);
        }
    }
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

