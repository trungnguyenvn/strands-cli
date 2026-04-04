//! MCP server configuration loading and connection management.
//!
//! Discovers MCP server definitions from `.strands/mcp.json` and `.claude/mcp.json`
//! (project-level and user-level), connects to each server, and collects their
//! tools as `Arc<dyn AgentTool>` — identical to native tools.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use colored::Colorize;
use serde::Deserialize;

use strands::tools::mcp::{MCPClient, MCPHttpClient, MCPHttpServerConfig, MCPServerConfig};
use strands::types::tools::AgentTool;

// ---------------------------------------------------------------------------
// Config file structs (serde layer)
// ---------------------------------------------------------------------------

/// Top-level structure of `mcp.json`.
#[derive(Debug, Deserialize, Default)]
pub struct McpConfigFile {
    #[serde(rename = "mcpServers", default)]
    pub mcp_servers: HashMap<String, McpServerEntry>,
}

/// One entry in the `mcpServers` map.
///
/// Presence of `url` selects HTTP transport; otherwise `command` + `args`
/// selects stdio transport.
#[derive(Debug, Deserialize)]
pub struct McpServerEntry {
    /// Executable for stdio transport (e.g. "npx", "uvx").
    pub command: Option<String>,
    /// Arguments for the stdio command.
    #[serde(default)]
    pub args: Vec<String>,

    /// Base URL for HTTP transport.
    pub url: Option<String>,
    /// HTTP headers (e.g. Authorization).
    #[serde(default)]
    pub headers: HashMap<String, String>,

    /// Environment variables for the subprocess.
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// Timeout in seconds (default: 30).
    pub timeout_secs: Option<u64>,

    /// If true, skip this server entirely.
    #[serde(default)]
    pub disabled: bool,
}

// ---------------------------------------------------------------------------
// Session — keeps MCP clients alive for the agent session
// ---------------------------------------------------------------------------

/// Per-server info for the `/mcp` command.
#[derive(Clone, Debug)]
pub struct McpServerInfo {
    pub name: String,
    pub transport: &'static str, // "stdio" or "http"
    pub tool_names: Vec<String>,
}

/// Holds connected MCP clients and their tools. Must stay alive for the
/// duration of the agent session — dropping it kills subprocesses / closes
/// HTTP sessions.
pub struct McpSession {
    /// Stdio-transport clients (kept alive; Drop kills subprocess).
    #[allow(dead_code)]
    pub stdio_clients: Vec<MCPClient>,
    /// HTTP-transport clients (kept alive; Drop closes session).
    #[allow(dead_code)]
    pub http_clients: Vec<MCPHttpClient>,
    /// Flat list of all discovered MCP tools.
    pub tools: Vec<Arc<dyn AgentTool>>,
    /// Names of successfully connected servers (for prompt rendering).
    pub server_names: Vec<String>,
    /// Per-server details for the `/mcp` command.
    pub servers: Vec<McpServerInfo>,
    /// Number of servers that failed to connect.
    pub failed_count: usize,
}

// ---------------------------------------------------------------------------
// Config discovery
// ---------------------------------------------------------------------------

/// Load and merge MCP config from standard locations.
///
/// Priority (higher wins on name collision):
/// 1. `~/.strands/mcp.json`, `~/.claude/mcp.json` (user-level)
/// 2. `<cwd>/.strands/mcp.json`, `<cwd>/.claude/mcp.json` (project-level)
fn load_mcp_config(cwd: &Path) -> McpConfigFile {
    let mut merged: HashMap<String, McpServerEntry> = HashMap::new();

    // User-level (lower priority — use or_insert)
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        for dir in &[".strands", ".claude"] {
            if let Some(cfg) = try_load_mcp_json(&home.join(dir).join("mcp.json")) {
                for (name, entry) in cfg.mcp_servers {
                    merged.entry(name).or_insert(entry);
                }
            }
        }
    }

    // Project-level (higher priority — insert overwrites)
    for dir in &[".strands", ".claude"] {
        if let Some(cfg) = try_load_mcp_json(&cwd.join(dir).join("mcp.json")) {
            for (name, entry) in cfg.mcp_servers {
                merged.insert(name, entry);
            }
        }
    }

    McpConfigFile {
        mcp_servers: merged,
    }
}

/// Try to load and parse a single mcp.json file. Returns None if the file
/// doesn't exist; prints a warning on parse errors.
fn try_load_mcp_json(path: &Path) -> Option<McpConfigFile> {
    let content = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str::<McpConfigFile>(&content) {
        Ok(cfg) => Some(cfg),
        Err(e) => {
            eprintln!(
                "{} Failed to parse {}: {}",
                "mcp warning:".yellow().bold(),
                path.display(),
                e
            );
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Connection
// ---------------------------------------------------------------------------

/// Discover MCP servers from config files, connect to each, and collect tools.
///
/// Servers that fail to connect are skipped with a warning — other servers
/// and native tools continue to work.
pub async fn load_mcp_servers(cwd: &Path, quiet: bool) -> McpSession {
    let config = load_mcp_config(cwd);

    if config.mcp_servers.is_empty() {
        return McpSession {
            stdio_clients: Vec::new(),
            http_clients: Vec::new(),
            tools: Vec::new(),
            server_names: Vec::new(),
            servers: Vec::new(),
            failed_count: 0,
        };
    }

    // Connect to all servers in parallel
    enum ConnectResult {
        Stdio {
            name: String,
            client: MCPClient,
            tools: Vec<Arc<dyn AgentTool>>,
            tool_names: Vec<String>,
        },
        Http {
            name: String,
            client: MCPHttpClient,
            tools: Vec<Arc<dyn AgentTool>>,
            tool_names: Vec<String>,
        },
        Failed(String, String),   // (name, error)
        Skipped(String),          // name — no command or url
    }

    let mut handles = Vec::new();

    for (name, entry) in config.mcp_servers {
        if entry.disabled {
            continue;
        }
        let timeout = Duration::from_secs(entry.timeout_secs.unwrap_or(30));

        if let Some(url) = entry.url {
            handles.push(tokio::spawn(async move {
                let sdk_config = MCPHttpServerConfig {
                    name: name.clone(),
                    base_url: url,
                    headers: entry.headers,
                    timeout,
                    env: entry.env,
                };
                let client = MCPHttpClient::new(sdk_config);
                match client.connect().await {
                    Ok(()) => {
                        let server_tools = client.list_tools().await;
                        let tool_names: Vec<String> = server_tools.iter().map(|t| t.tool_name().to_string()).collect();
                        ConnectResult::Http { name, client, tools: server_tools, tool_names }
                    }
                    Err(e) => ConnectResult::Failed(name, e.to_string()),
                }
            }));
        } else if let Some(command) = entry.command {
            handles.push(tokio::spawn(async move {
                let mut cmd = vec![command];
                cmd.extend(entry.args);
                let sdk_config = MCPServerConfig {
                    name: name.clone(),
                    command: cmd,
                    env: entry.env,
                    timeout,
                };
                let client = MCPClient::new(sdk_config);
                match client.connect().await {
                    Ok(()) => {
                        let server_tools = client.list_tools().await;
                        let tool_names: Vec<String> = server_tools.iter().map(|t| t.tool_name().to_string()).collect();
                        ConnectResult::Stdio { name, client, tools: server_tools, tool_names }
                    }
                    Err(e) => ConnectResult::Failed(name, e.to_string()),
                }
            }));
        } else {
            handles.push(tokio::spawn(async move {
                ConnectResult::Skipped(name)
            }));
        }
    }

    let mut stdio_clients = Vec::new();
    let mut http_clients = Vec::new();
    let mut tools: Vec<Arc<dyn AgentTool>> = Vec::new();
    let mut server_names = Vec::new();
    let mut servers = Vec::new();
    let mut failed_count: usize = 0;

    for handle in handles {
        let Ok(result) = handle.await else {
            failed_count += 1;
            continue;
        };
        match result {
            ConnectResult::Http { name, client, tools: t, tool_names } => {
                let n = tool_names.len();
                if !quiet {
                    eprintln!("{} Connected to HTTP MCP server '{}' ({} tools)", "mcp:".cyan().bold(), name, n);
                }
                tools.extend(t);
                server_names.push(name.clone());
                servers.push(McpServerInfo { name, transport: "http", tool_names });
                http_clients.push(client);
            }
            ConnectResult::Stdio { name, client, tools: t, tool_names } => {
                let n = tool_names.len();
                if !quiet {
                    eprintln!("{} Connected to MCP server '{}' ({} tools)", "mcp:".cyan().bold(), name, n);
                }
                tools.extend(t);
                server_names.push(name.clone());
                servers.push(McpServerInfo { name, transport: "stdio", tool_names });
                stdio_clients.push(client);
            }
            ConnectResult::Failed(name, e) => {
                if !quiet {
                    eprintln!("{} Failed to connect to MCP server '{}': {}", "mcp warning:".yellow().bold(), name, e);
                }
                failed_count += 1;
            }
            ConnectResult::Skipped(name) => {
                if !quiet {
                    eprintln!("{} MCP server '{}' has neither 'command' nor 'url' — skipping", "mcp warning:".yellow().bold(), name);
                }
                failed_count += 1;
            }
        }
    }

    McpSession {
        stdio_clients,
        http_clients,
        tools,
        server_names,
        servers,
        failed_count,
    }
}
