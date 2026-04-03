//! Strands CLI — Interactive REPL for Strands Agents
//!
//! A minimal, streaming CLI that wires core coding tools (shell, file read/write/edit,
//! glob, grep, think) to a configurable model provider (Anthropic or Bedrock).

use std::io::{self, Write as _};
use std::sync::Arc;

use clap::Parser;
use colored::Colorize;
use futures::StreamExt;
use serde_json::json;

// Streaming event types (used via string matching for flexibility)
use strands::types::tools::{AgentTool, ToolResult, ToolUse};
use strands::tools::FunctionTool;
use strands::{Agent, Result};

// Tools from strands-tools
use strands_tools::{FileReadTool, FileWriteTool, FileEditTool, GlobTool, GrepTool};
use strands_tools::advanced::ThinkTool;
use strands_tools::system::ShellTool;

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

    /// Run a single prompt (non-interactive)
    #[arg(long = "prompt")]
    oneshot: Option<String>,
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Build model
    let model = build_model(&cli).await?;

    // Build tools
    let tools = build_tools();

    // System prompt
    let system_prompt = cli.system.clone().unwrap_or_else(|| build_system_prompt(&tools));

    // Build agent
    let agent = Agent::builder()
        .with_model(model)
        .with_system_prompt(system_prompt)
        .with_tools(tools)
        .with_max_iterations(cli.max_iterations)
        .with_sliding_window(500)
        .build()
        .await?;

    // One-shot or REPL
    if let Some(prompt) = &cli.oneshot {
        run_single_turn(&agent, prompt).await?;
    } else {
        run_repl(&agent).await?;
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

            let model_id = cli.model.clone()
                .unwrap_or_else(|| "claude-sonnet-4-20250514".to_string());

            let config = AnthropicConfig {
                model_id: model_id.clone(),
                max_tokens: Some(16384),
                ..Default::default()
            };

            let api_key = std::env::var("ANTHROPIC_API_KEY").ok();
            let model = AnthropicModel::new(
                Some(model_id),
                api_key,
                None,
                config,
            ).await?;

            Ok(Arc::new(model))
        }
        "bedrock" => {
            use strands::models::bedrock::{BedrockModel, BedrockConfig};

            let mut config = BedrockConfig::default();
            if let Some(ref model_id) = cli.model {
                config.model_id = model_id.clone();
            }
            config.max_tokens = Some(16384);

            let model = BedrockModel::new(None, None, None, config).await?;

            Ok(Arc::new(model))
        }
        other => {
            eprintln!("{} Unknown provider '{}'. Use 'anthropic' or 'bedrock'.", "error:".red().bold(), other);
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
    }).as_object().unwrap().iter().map(|(k, v)| (k.clone(), v.clone())).collect();

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
    let command = tool_use.input.get("command").and_then(|v| v.as_str())
        .ok_or_else(|| strands::Error::ToolExecution("Missing 'command' parameter".into()))?;

    let _timeout_secs = tool_use.input.get("timeout")
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
        (&["grep ", "rg "],          "Use the Grep tool instead of grep/rg via Bash."),
        (&["cat ", "head ", "tail "], "Use the Read tool instead of cat/head/tail via Bash."),
        (&["find "],                  "Use the Glob tool instead of find via Bash."),
        (&["sed ", "awk "],           "Use the Edit tool instead of sed/awk via Bash."),
    ];
    for (patterns, msg) in redirects {
        if patterns.iter().any(|p| command.starts_with(p)) {
            return Ok(ToolResult::error(tool_use.tool_use_id.clone(), msg.to_string()));
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
                if !result.is_empty() { result.push('\n'); }
                result.push_str("stderr:\n");
                result.push_str(&stderr);
            }
            if !output.status.success() {
                result.push_str(&format!("\nExit code: {}", output.status.code().unwrap_or(-1)));
            }
            // Truncate large output
            if result.len() > 30_000 {
                result.truncate(30_000);
                result.push_str("\n... (output truncated at 30KB)");
            }
            if result.is_empty() { result = "(no output)".into(); }
            Ok(ToolResult::success(tool_use.tool_use_id.clone(), result))
        }
        Err(e) => Ok(ToolResult::error(
            tool_use.tool_use_id.clone(),
            format!("Failed to execute command: {}", e),
        )),
    }
}

// ---------------------------------------------------------------------------
// System prompt
// ---------------------------------------------------------------------------

fn build_system_prompt(tools: &[Arc<dyn AgentTool>]) -> String {
    let tool_names: Vec<String> = tools.iter().map(|t| t.tool_name().to_string()).collect();
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".into());

    format!(
r#"You are an expert software engineer working as an interactive coding assistant.

# Tools
Available tools: {tools}

Use the dedicated tool for each operation:
- Read files: Read (not cat/head/tail via Bash)
- Edit files: Edit (not sed/awk via Bash)
- Write files: Write (not echo/heredoc via Bash)
- Search file contents: Grep (not grep/rg via Bash)
- Find files by pattern: Glob (not find/ls via Bash)
- Run shell commands: Bash (only for commands without a dedicated tool)
- Structured reasoning: Think (use for complex multi-step reasoning)

# Guidelines
- Read code before modifying it.
- Be concise. Lead with the answer, not the reasoning.
- When editing, prefer small targeted changes over full rewrites.
- Use absolute paths based on the working directory.

# Environment
- Working directory: {cwd}
- Platform: {platform}
- Shell: bash"#,
        tools = tool_names.join(", "),
        cwd = cwd,
        platform = std::env::consts::OS,
    )
}

// ---------------------------------------------------------------------------
// REPL
// ---------------------------------------------------------------------------

async fn run_repl(agent: &Agent) -> Result<()> {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".into());

    println!("{}", "Strands CLI".bold());
    println!("  cwd: {}", cwd.dimmed());
    println!("  Type {} to quit, {} to clear history\n",
        "/exit".yellow(), "/clear".yellow());

    let stdin = io::stdin();
    loop {
        // Prompt
        print!("{} ", ">".cyan().bold());
        io::stdout().flush().unwrap();

        // Read input
        let mut line = String::new();
        if stdin.read_line(&mut line).unwrap() == 0 {
            break; // EOF
        }
        let input = line.trim();
        if input.is_empty() { continue; }

        // Commands
        match input {
            "/exit" | "/quit" => break,
            "/clear" => {
                agent.clear_history();
                println!("{}", "Conversation cleared.".dimmed());
                continue;
            }
            _ => {}
        }

        // Stream response
        if let Err(e) = stream_turn(agent, input).await {
            eprintln!("\n{} {}", "error:".red().bold(), e);
        }
        println!(); // blank line between turns
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Streaming a single turn
// ---------------------------------------------------------------------------

async fn run_single_turn(agent: &Agent, prompt: &str) -> Result<()> {
    stream_turn(agent, prompt).await
}

async fn stream_turn(agent: &Agent, prompt: &str) -> Result<()> {
    let mut stream = agent.stream_async(prompt).await?;
    let mut in_text = false;

    while let Some(event) = stream.next().await {
        let ev = event?;
        let event_type_str = ev.get("event_type").and_then(|v| v.as_str()).unwrap_or("");

        match event_type_str {
            // Text streaming
            "content_block_delta" => {
                if let Some(text) = ev.pointer("/delta/text").and_then(|v| v.as_str()) {
                    if !in_text {
                        in_text = true;
                    }
                    print!("{}", text);
                    io::stdout().flush().unwrap();
                }
            }

            // Tool call start
            "content_block_start" => {
                if let Some(name) = ev.pointer("/content_block/name").and_then(|v| v.as_str()) {
                    if in_text {
                        println!();
                        in_text = false;
                    }
                    print!("{}", format!("  {} {}", "tool:".dimmed(), name.yellow()));
                    io::stdout().flush().unwrap();
                }
            }

            // Tool call with input
            "tool_call" => {
                let name = ev.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                let input = &ev["input"];

                // Show a compact summary of the tool call
                let summary = tool_call_summary(name, input);
                println!(" {}", summary.dimmed());
            }

            // Tool result
            "tool_result" => {
                let status = ev.get("status").and_then(|v| v.as_str()).unwrap_or("?");
                let content = ev.get("content").and_then(|v| v.as_str()).unwrap_or("");
                let preview = if content.len() > 200 {
                    format!("{}...", &content[..200])
                } else {
                    content.to_string()
                };
                let color = if status == "success" { "32" } else { "31" };
                let first_line = preview.lines().next().unwrap_or("");
                println!("  \x1b[{}m{} {}\x1b[0m", color, "result:".to_string(), first_line);
            }

            // End of message
            "message_stop" => {
                if in_text {
                    println!();
                }
                break;
            }

            // Data field (simple streaming)
            _ => {
                if let Some(data) = ev.get("data").and_then(|d| d.as_str()) {
                    if !data.is_empty() {
                        if !in_text { in_text = true; }
                        print!("{}", data);
                        io::stdout().flush().unwrap();
                    }
                }
            }
        }
    }

    Ok(())
}

/// Produce a short one-line summary of a tool call for display.
fn tool_call_summary(name: &str, input: &serde_json::Value) -> String {
    match name {
        "Bash" | "Shell" => {
            let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("?");
            let display = if cmd.len() > 80 { format!("{}...", &cmd[..80]) } else { cmd.to_string() };
            format!("$ {}", display)
        }
        "Read" => {
            let path = input.get("file_path").or(input.get("path"))
                .and_then(|v| v.as_str()).unwrap_or("?");
            format!("{}", path)
        }
        "Write" => {
            let path = input.get("file_path").and_then(|v| v.as_str()).unwrap_or("?");
            format!("{}", path)
        }
        "Edit" => {
            let path = input.get("file_path").and_then(|v| v.as_str()).unwrap_or("?");
            format!("{}", path)
        }
        "Glob" => {
            let pattern = input.get("pattern").and_then(|v| v.as_str()).unwrap_or("?");
            format!("{}", pattern)
        }
        "Grep" => {
            let pattern = input.get("pattern").and_then(|v| v.as_str()).unwrap_or("?");
            let path = input.get("path").and_then(|v| v.as_str()).unwrap_or(".");
            format!("/{}/  in {}", pattern, path)
        }
        "Think" => {
            let thought = input.get("thought").and_then(|v| v.as_str()).unwrap_or("");
            let preview = if thought.len() > 60 { format!("{}...", &thought[..60]) } else { thought.to_string() };
            format!("\"{}\"", preview)
        }
        _ => {
            let s = input.to_string();
            if s.len() > 80 { format!("{}...", &s[..80]) } else { s }
        }
    }
}
