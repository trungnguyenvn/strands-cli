//! Plain-text REPL fallback (no TUI, prints directly to stdout).
//!
//! Used when `--no-tui` is passed or when stdout is not a terminal.

use std::io::{self, Write as _};

use colored::Colorize;
use futures::StreamExt;

use strands::Agent;

pub async fn run_repl(agent: &Agent) -> strands::Result<()> {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".into());

    println!("{}", "Strands CLI".bold());
    println!("  cwd: {}", cwd.dimmed());
    println!(
        "  Type {} to quit, {} to clear history\n",
        "/exit".yellow(),
        "/clear".yellow()
    );

    let stdin = io::stdin();
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

        match input {
            "/exit" | "/quit" => break,
            "/clear" => {
                agent.clear_history();
                println!("{}", "Conversation cleared.".dimmed());
                continue;
            }
            _ => {}
        }

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
