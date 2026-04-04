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
}

// ---------------------------------------------------------------------------
// Session — keeps MCP clients alive for the agent session
// ---------------------------------------------------------------------------

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
pub async fn load_mcp_servers(cwd: &Path) -> McpSession {
    let config = load_mcp_config(cwd);

    if config.mcp_servers.is_empty() {
        return McpSession {
            stdio_clients: Vec::new(),
            http_clients: Vec::new(),
            tools: Vec::new(),
            server_names: Vec::new(),
        };
    }

    let mut stdio_clients = Vec::new();
    let mut http_clients = Vec::new();
    let mut tools: Vec<Arc<dyn AgentTool>> = Vec::new();
    let mut server_names = Vec::new();

    for (name, entry) in config.mcp_servers {
        let timeout = Duration::from_secs(entry.timeout_secs.unwrap_or(30));

        if let Some(url) = entry.url {
            // HTTP transport
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
                    let n = server_tools.len();
                    tools.extend(server_tools);
                    server_names.push(name.clone());
                    eprintln!(
                        "{} Connected to HTTP MCP server '{}' ({} tools)",
                        "mcp:".cyan().bold(),
                        name,
                        n
                    );
                    http_clients.push(client);
                }
                Err(e) => {
                    eprintln!(
                        "{} Failed to connect to HTTP MCP server '{}': {}",
                        "mcp warning:".yellow().bold(),
                        name,
                        e
                    );
                }
            }
        } else if let Some(command) = entry.command {
            // Stdio transport
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
                    let n = server_tools.len();
                    tools.extend(server_tools);
                    server_names.push(name.clone());
                    eprintln!(
                        "{} Connected to MCP server '{}' ({} tools)",
                        "mcp:".cyan().bold(),
                        name,
                        n
                    );
                    stdio_clients.push(client);
                }
                Err(e) => {
                    eprintln!(
                        "{} Failed to connect to MCP server '{}': {}",
                        "mcp warning:".yellow().bold(),
                        name,
                        e
                    );
                }
            }
        } else {
            eprintln!(
                "{} MCP server '{}' has neither 'command' nor 'url' — skipping",
                "mcp warning:".yellow().bold(),
                name
            );
        }
    }

    McpSession {
        stdio_clients,
        http_clients,
        tools,
        server_names,
    }
}
