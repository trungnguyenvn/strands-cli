//! Plain-text REPL fallback (no TUI, prints directly to stdout).
//!
//! Used when `--no-tui` is passed or when stdout is not a terminal.

use std::io::{self, Write as _};

use colored::Colorize;
use futures::StreamExt;

use strands::Agent;

use crate::commands::{
    self, CommandContext, CommandRegistry, CommandResult, DispatchResult,
};

pub async fn run_repl(agent: &Agent, registry: CommandRegistry, mcp_servers: Vec<crate::mcp::McpServerInfo>) -> strands::Result<()> {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".into());

    println!("{}", "Strands CLI".bold());
    println!("  cwd: {}", cwd.dimmed());
    println!(
        "  Type {} for commands, {} to quit, {} to clear history\n",
        "/help".yellow(),
        "/exit".yellow(),
        "/clear".yellow()
    );

    let stdin = io::stdin();
    let mut turn_count: usize = 0;
    let mut message_count: usize = 0;

    loop {
        print!("{} ", ">".cyan().bold());
        io::stdout().flush().unwrap();

        let mut line = String::new();
        if stdin.read_line(&mut line).unwrap() == 0 {
            break;
        }
        let input = line.trim();
        if input.is_empty() {
            continue;
        }

        // Dispatch slash commands via registry
        if input.starts_with('/') {
            let ctx = CommandContext {
                model_name: String::new(), // not available in plain REPL
                turn_count,
                message_count,
                all_commands: registry.command_infos(),
                mcp_servers: mcp_servers.clone(),
            };
            match commands::dispatch(input, &registry, &ctx) {
                DispatchResult::Local(CommandResult::Quit) => break,
                DispatchResult::Local(CommandResult::Clear) => {
                    agent.clear_history();
                    message_count = 0;
                    println!("{}", "Conversation cleared.".dimmed());
                    continue;
                }
                DispatchResult::Local(CommandResult::Text(text)) => {
                    println!("{}", text);
                    continue;
                }
                DispatchResult::Local(CommandResult::Skip) => continue,
                DispatchResult::Local(CommandResult::ModelPicker { current_model, items }) => {
                    // Plain REPL fallback: show numbered list, let user pick
                    println!("Current model: {}\n", current_model);
                    for (i, item) in items.iter().enumerate() {
                        let marker = if item.model_id == current_model || item.alias == current_model {
                            "✓"
                        } else {
                            " "
                        };
                        println!("  {} {:2}) {:<18} {}", marker, i + 1, item.alias, item.label);
                    }
                    print!("\nSelect (1-{}) or Enter to cancel: ", items.len());
                    io::stdout().flush().unwrap();
                    let mut choice = String::new();
                    if stdin.read_line(&mut choice).unwrap() == 0 {
                        break;
                    }
                    if let Ok(n) = choice.trim().parse::<usize>() {
                        if n >= 1 && n <= items.len() {
                            let model_id = &items[n - 1].model_id;
                            println!("Switching model to {}...", model_id);
                            match crate::build_model_by_id(model_id).await {
                                Ok(new_model) => {
                                    agent.swap_model(new_model);
                                    println!("{}", format!("Model switched to {}", model_id).green());
                                }
                                Err(e) => {
                                    eprintln!("{} {}", "error:".red().bold(), e);
                                }
                            }
                        }
                    }
                    continue;
                }
                DispatchResult::Local(CommandResult::ModeSwitch(mode_name)) => {
                    use strands_tools::utility::mode_skills;
                    match mode_skills::handle_mode_skill(&mode_name, None) {
                        Some(Ok(result)) => {
                            if let Some(msg) = result.content {
                                println!("{}", msg.green());
                            }
                        }
                        Some(Err(e)) => eprintln!("{} {}", "error:".red().bold(), e),
                        None => eprintln!("{} Unknown mode: {}", "error:".red().bold(), mode_name),
                    }
                    continue;
                }
                DispatchResult::Local(CommandResult::SwitchModel(model_id)) => {
                    println!("Switching model to {}...", model_id);
                    match crate::build_model_by_id(&model_id).await {
                        Ok(new_model) => {
                            agent.swap_model(new_model);
                            println!("{}", format!("Model switched to {}", model_id).green());
                        }
                        Err(e) => {
                            eprintln!("{} {}", "error:".red().bold(), e);
                        }
                    }
                    continue;
                }
                DispatchResult::Prompt(expanded) => {
                    turn_count += 1;
                    message_count += 2;
                    if let Err(e) = stream_turn(agent, &expanded).await {
                        eprintln!("\n{} {}", "error:".red().bold(), e);
                    }
                    println!();
                    continue;
                }
                DispatchResult::Unknown(name) => {
                    eprintln!(
                        "{} Unknown command: /{}. Type /help for available commands.",
                        "error:".red().bold(),
                        name
                    );
                    continue;
                }
                DispatchResult::NotACommand => {
                    // Fall through — treat as normal input
                }
            }
        }

        turn_count += 1;
        message_count += 2;
        if let Err(e) = stream_turn(agent, input).await {
            eprintln!("\n{} {}", "error:".red().bold(), e);
        }
        println!();
    }

    Ok(())
}

pub async fn run_single_turn(agent: &Agent, prompt: &str) -> strands::Result<()> {
    stream_turn(agent, prompt).await
}

async fn stream_turn(agent: &Agent, prompt: &str) -> strands::Result<()> {
    let mut stream = agent.stream_async(prompt).await?;
    let mut in_text = false;

    while let Some(event) = stream.next().await {
        let ev = event?;
        let event_type_str = ev.get("event_type").and_then(|v| v.as_str()).unwrap_or("");

        match event_type_str {
            "content_block_delta" => {
                if let Some(text) = ev.pointer("/delta/text").and_then(|v| v.as_str()) {
                    if !in_text {
                        in_text = true;
                    }
                    print!("{}", text);
                    io::stdout().flush().unwrap();
                }
            }
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
            "tool_call" => {
                let name = ev.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                let input = &ev["input"];
                let summary = tool_call_summary(name, input);
                println!(" {}", summary.dimmed());
            }
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
                println!("  \x1b[{}m{} {}\x1b[0m", color, "result:", first_line);
            }
            "message_stop" => {
                if in_text {
                    println!();
                }
                break;
            }
            _ => {
                if let Some(data) = ev.get("data").and_then(|d| d.as_str()) {
                    if !data.is_empty() {
                        if !in_text {
                            in_text = true;
                        }
                        print!("{}", data);
                        io::stdout().flush().unwrap();
                    }
                }
            }
        }
    }

    Ok(())
}

pub fn tool_call_summary(name: &str, input: &serde_json::Value) -> String {
    match name {
        "Bash" | "Shell" => {
            let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("?");
            let display = if cmd.len() > 80 {
                format!("{}...", &cmd[..80])
            } else {
                cmd.to_string()
            };
            format!("$ {}", display)
        }
        "Read" => {
            let path = input
                .get("file_path")
                .or(input.get("path"))
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            path.to_string()
        }
        "Write" | "Edit" => {
            let path = input.get("file_path").and_then(|v| v.as_str()).unwrap_or("?");
            path.to_string()
        }
        "Glob" => {
            let pattern = input.get("pattern").and_then(|v| v.as_str()).unwrap_or("?");
            pattern.to_string()
        }
        "Grep" => {
            let pattern = input.get("pattern").and_then(|v| v.as_str()).unwrap_or("?");
            let path = input.get("path").and_then(|v| v.as_str()).unwrap_or(".");
            format!("/{}/  in {}", pattern, path)
        }
        "Think" => {
            let thought = input.get("thought").and_then(|v| v.as_str()).unwrap_or("");
            let preview = if thought.len() > 60 {
                format!("{}...", &thought[..60])
            } else {
                thought.to_string()
            };
            format!("\"{}\"", preview)
        }
        _ => {
            let s = input.to_string();
            if s.len() > 80 {
                format!("{}...", &s[..80])
            } else {
                s
            }
        }
    }
}
