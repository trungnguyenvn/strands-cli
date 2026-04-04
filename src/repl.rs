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
    let mut pending_system_reminder: Option<String> = None;

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
            let messages_json: Vec<serde_json::Value> = agent
                .get_messages()
                .iter()
                .filter_map(|m| serde_json::to_value(m).ok())
                .collect();
            let ctx = CommandContext {
                model_name: String::new(), // not available in plain REPL
                turn_count,
                message_count,
                all_commands: registry.command_infos(),
                mcp_servers: mcp_servers.clone(),
                token_counts: agent.token_counts(),
                context_percent_used: agent.context_percent_used(),
                system_prompt: String::new(),
                tool_specs: Vec::new(),
                mcp_tool_specs: Vec::new(),
                memory_files: Vec::new(),
                skills: Vec::new(),
                messages_json,
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
                    use strands_tools::utility::{mode_skills, plan_state};
                    match mode_skills::handle_mode_skill(&mode_name, None) {
                        Some(Ok(result)) => {
                            if let Some(msg) = result.content {
                                println!("{}", msg.green());
                            }
                            // If entering plan mode, prepare system-reminder for next prompt
                            if mode_name == "plan" && plan_state::is_in_plan_mode() {
                                let plan_file = plan_state::get_plan_file_path(None);
                                pending_system_reminder = Some(
                                    plan_state::build_plan_mode_system_reminder(&plan_file)
                                );
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
                DispatchResult::Local(CommandResult::ResumeSession(session_ref)) => {
                    let cwd = std::env::current_dir().unwrap_or_default();
                    let sessions_dir = crate::session::SessionId::storage_dir(&cwd);
                    match crate::session::resolve_and_load(&sessions_dir, &session_ref).await {
                        Ok((id, msgs)) => {
                            agent.clear_history();
                            for m in &msgs {
                                agent.add_message(m.clone());
                            }
                            message_count = msgs.len();
                            println!("{}", format!("Resumed session {} ({} messages)", id, msgs.len()).green());
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
                DispatchResult::CompactPrompt(expanded) => {
                    turn_count += 1;
                    message_count += 2;
                    match stream_turn(agent, &expanded).await {
                        Ok(_) => {
                            // Replace history with the summary
                            agent.replace_history_with_summary(&expanded);
                            println!("{}", "\nConversation compacted.".dimmed());
                        }
                        Err(e) => eprintln!("\n{} {}", "error:".red().bold(), e),
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

        // Prepend any pending system-reminder (e.g. plan mode instructions)
        let agent_prompt = if let Some(reminder) = pending_system_reminder.take() {
            format!("{}\n\n{}", reminder, input)
        } else {
            input.to_string()
        };

        match stream_turn(agent, &agent_prompt).await {
            Ok(did_exit_plan) => {
                if did_exit_plan {
                    // Set system context for next turn with plan file reference
                    let plan_file = strands_tools::utility::plan_state::get_plan_file_path(None);
                    pending_system_reminder = Some(format!(
                        "<system-reminder>\n\
                         ## Exited Plan Mode\n\n\
                         You have exited plan mode. You can now make edits, run tools, and take actions. \
                         The plan file is located at {} if you need to reference it.\n\
                         </system-reminder>",
                        plan_file.display()
                    ));
                }
            }
            Err(e) => {
                eprintln!("\n{} {}", "error:".red().bold(), e);
            }
        }
        println!();
    }

    Ok(())
}

pub async fn run_single_turn(agent: &Agent, prompt: &str) -> strands::Result<()> {
    stream_turn(agent, prompt).await.map(|_| ())
}

/// Returns true if ExitPlanMode was called (plan mode exited during this turn).
async fn stream_turn(agent: &Agent, prompt: &str) -> strands::Result<bool> {
    let mut stream = agent.stream_async(prompt).await?;
    let mut in_text = false;
    let mut plan_mode_exited = false;

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
                let tool_name = ev.get("tool_name").and_then(|v| v.as_str()).unwrap_or("");
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

                // ExitPlanMode should abort the agent loop (matching Claude Code).
                // Also clear history so plan mode system-reminder doesn't persist.
                if tool_name == "ExitPlanMode" && status == "success" {
                    agent.cancel();
                    plan_mode_exited = true;
                }
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

    // After plan mode exit: show plan, clear history, exit plan mode
    if plan_mode_exited {
        use strands_tools::utility::plan_state;

        let plan_file = plan_state::get_plan_file_path(None);
        let plan_content = std::fs::read_to_string(&plan_file).unwrap_or_default();

        agent.clear_history();
        agent.reset_cancel();

        // Exit plan mode properly
        if plan_state::is_in_plan_mode() {
            let _ = plan_state::exit_plan_mode(None);
        }

        // Show plan to user
        if !plan_content.trim().is_empty() {
            println!("\n{}", "─".repeat(60));
            println!("{} ({})\n", "Plan".bold(), plan_file.display());
            println!("{}", plan_content.trim());
            println!("{}", "─".repeat(60));
            println!("{}", "Type your message to approve and start, or provide feedback.".dimmed());
        }
    }

    Ok(plan_mode_exited)
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
