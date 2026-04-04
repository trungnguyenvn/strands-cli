//! Context window usage analysis — mirrors Claude Code's `analyzeContext.ts`.
//!
//! Estimates token usage across categories (system prompt, tools, messages, memory
//! files, MCP tools) and formats a breakdown table for the `/context` command.

// ---------------------------------------------------------------------------
// Token estimation
// ---------------------------------------------------------------------------

/// Rough token count estimate: chars / 4.
/// Matches the heuristic used by the Rust SDK's `ProactiveConversationManager`.
fn estimate_tokens(text: &str) -> u64 {
    (text.len() as u64 + 3) / 4
}

/// Estimate tokens for a JSON-serialized value (tool specs, messages, etc.).
fn estimate_tokens_json(value: &serde_json::Value) -> u64 {
    let s = serde_json::to_string(value).unwrap_or_default();
    estimate_tokens(&s)
}

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// A category of context window usage.
#[derive(Clone, Debug)]
pub struct ContextCategory {
    pub name: String,
    pub tokens: u64,
    pub percentage: f64,
}

/// Info about a loaded memory file (STRANDS.md / CLAUDE.md).
#[derive(Clone, Debug)]
pub struct MemoryFileInfo {
    pub path: String,
    pub source_type: String,
    pub tokens: u64,
}

/// Info about an MCP tool.
#[derive(Clone, Debug)]
pub struct McpToolInfo {
    pub name: String,
    pub server_name: String,
    pub tokens: u64,
}

/// A suggestion for improving context usage.
#[derive(Clone, Debug)]
pub struct ContextSuggestion {
    pub message: String,
    pub severity: SuggestionSeverity,
}

#[derive(Clone, Debug)]
pub enum SuggestionSeverity {
    Info,
    Warning,
    Critical,
}

/// Info about a loaded skill.
#[derive(Clone, Debug)]
pub struct SkillDetail {
    pub name: String,
    pub source: String,
    pub tokens: u64,
}

/// Full context analysis result.
#[derive(Clone, Debug)]
pub struct ContextData {
    pub categories: Vec<ContextCategory>,
    pub total_tokens: u64,
    pub max_tokens: u64,
    pub percentage: f64,
    pub model: String,
    pub memory_files: Vec<MemoryFileInfo>,
    pub mcp_tools: Vec<McpToolInfo>,
    pub skills: Vec<SkillDetail>,
    pub suggestions: Vec<ContextSuggestion>,
    /// Actual token counts from the SDK tracker, if available.
    pub api_token_counts: Option<(u64, u64)>,
}

// ---------------------------------------------------------------------------
// Input for analysis
// ---------------------------------------------------------------------------

/// Summary of a tool's definition for token estimation.
#[derive(Clone, Debug)]
pub struct ToolSpecSummary {
    pub name: String,
    pub description: String,
    pub input_schema_json: String,
}

/// Summary of a skill for token estimation.
#[derive(Clone, Debug)]
pub struct SkillSummary {
    pub name: String,
    pub description: String,
    /// The skill body content (SKILL.md).
    pub content: String,
    /// Source: "project" or "user".
    pub source: String,
}

/// Everything needed to run context analysis.
pub struct AnalysisInput {
    /// Current model name.
    pub model_name: String,
    /// The rendered system prompt text.
    pub system_prompt: String,
    /// Tool spec summaries for built-in tools.
    pub tool_specs: Vec<ToolSpecSummary>,
    /// MCP tool specs: (tool_name, server_name, spec_json).
    pub mcp_tool_specs: Vec<(String, String, String)>,
    /// Loaded memory files: (path, source_type, content).
    pub memory_files: Vec<(String, String, String)>,
    /// Loaded skills: (name, description, content, source).
    pub skills: Vec<SkillSummary>,
    /// Conversation messages as JSON values (from agent.get_messages()).
    pub messages_json: Vec<serde_json::Value>,
    /// Token counts from the SDK tracker: (used, limit).
    pub sdk_token_counts: Option<(u64, u64)>,
    /// Context percent used from the SDK tracker.
    #[allow(dead_code)]
    pub sdk_context_percent: Option<f64>,
}

// ---------------------------------------------------------------------------
// Analysis
// ---------------------------------------------------------------------------

/// Analyze context window usage and produce a full breakdown.
pub fn analyze_context_usage(input: &AnalysisInput) -> ContextData {
    let max_tokens = input
        .sdk_token_counts
        .map(|(_, limit)| limit)
        .unwrap_or(200_000);

    // 1. System prompt tokens
    let system_prompt_tokens = estimate_tokens(&input.system_prompt);

    // 2. Built-in tool tokens
    let mut builtin_tool_tokens: u64 = 0;
    for spec in &input.tool_specs {
        let combined = format!(
            "{{\"name\":\"{}\",\"description\":\"{}\",\"input_schema\":{}}}",
            spec.name, spec.description, spec.input_schema_json
        );
        builtin_tool_tokens += estimate_tokens(&combined);
    }

    // 3. MCP tool tokens
    let mut mcp_tools = Vec::new();
    let mut mcp_tool_tokens: u64 = 0;
    for (name, server, spec_json) in &input.mcp_tool_specs {
        let tokens = estimate_tokens(spec_json);
        mcp_tool_tokens += tokens;
        mcp_tools.push(McpToolInfo {
            name: name.clone(),
            server_name: server.clone(),
            tokens,
        });
    }

    // 4. Memory file tokens
    let mut memory_files = Vec::new();
    let mut memory_tokens: u64 = 0;
    for (path, source_type, content) in &input.memory_files {
        let tokens = estimate_tokens(content);
        memory_tokens += tokens;
        memory_files.push(MemoryFileInfo {
            path: path.clone(),
            source_type: source_type.clone(),
            tokens,
        });
    }

    // 5. Skill tokens
    let mut skills = Vec::new();
    let mut skill_tokens: u64 = 0;
    for skill in &input.skills {
        // Skills contribute tokens via their frontmatter in the system prompt
        // (name + description + when_to_use) and their body content when invoked.
        // We estimate based on the frontmatter portion that's always loaded.
        let frontmatter = format!(
            "- {}: {}{}",
            skill.name,
            skill.description,
            skill.content.chars().take(200).collect::<String>(),
        );
        let tokens = estimate_tokens(&frontmatter);
        skill_tokens += tokens;
        skills.push(SkillDetail {
            name: skill.name.clone(),
            source: skill.source.clone(),
            tokens,
        });
    }

    // 6. Message tokens
    let mut message_tokens: u64 = 0;
    for msg in &input.messages_json {
        message_tokens += estimate_tokens_json(msg);
    }

    // Build categories
    let estimated_total = system_prompt_tokens
        + builtin_tool_tokens
        + mcp_tool_tokens
        + memory_tokens
        + skill_tokens
        + message_tokens;

    // Prefer SDK-reported total over our estimate
    let total_tokens = input
        .sdk_token_counts
        .map(|(used, _)| used)
        .unwrap_or(estimated_total);

    let percentage = if max_tokens > 0 {
        (total_tokens as f64 / max_tokens as f64) * 100.0
    } else {
        0.0
    };

    let mut categories = Vec::new();

    let add_category = |cats: &mut Vec<ContextCategory>, name: &str, tokens: u64| {
        let pct = if max_tokens > 0 {
            (tokens as f64 / max_tokens as f64) * 100.0
        } else {
            0.0
        };
        cats.push(ContextCategory {
            name: name.to_string(),
            tokens,
            percentage: pct,
        });
    };

    add_category(&mut categories, "System prompt", system_prompt_tokens);
    if !input.tool_specs.is_empty() {
        add_category(
            &mut categories,
            &format!("Tools ({})", input.tool_specs.len()),
            builtin_tool_tokens,
        );
    }
    if !input.mcp_tool_specs.is_empty() {
        add_category(
            &mut categories,
            &format!("MCP tools ({})", input.mcp_tool_specs.len()),
            mcp_tool_tokens,
        );
    }
    if !input.memory_files.is_empty() {
        add_category(
            &mut categories,
            &format!("Memory files ({})", input.memory_files.len()),
            memory_tokens,
        );
    }
    if !input.skills.is_empty() {
        add_category(
            &mut categories,
            &format!("Skills ({})", input.skills.len()),
            skill_tokens,
        );
    }
    if !input.messages_json.is_empty() {
        add_category(
            &mut categories,
            &format!("Messages ({})", input.messages_json.len()),
            message_tokens,
        );
    }

    // Free space as a category
    let free = max_tokens.saturating_sub(total_tokens);
    add_category(&mut categories, "Available", free);

    // Generate suggestions
    let suggestions = generate_suggestions(percentage, &categories, max_tokens);

    ContextData {
        categories,
        total_tokens,
        max_tokens,
        percentage,
        model: input.model_name.clone(),
        memory_files,
        mcp_tools,
        skills,
        suggestions,
        api_token_counts: input.sdk_token_counts,
    }
}

// ---------------------------------------------------------------------------
// Suggestions
// ---------------------------------------------------------------------------

fn generate_suggestions(
    percentage: f64,
    categories: &[ContextCategory],
    _max_tokens: u64,
) -> Vec<ContextSuggestion> {
    let mut suggestions = Vec::new();

    if percentage >= 90.0 {
        suggestions.push(ContextSuggestion {
            message: "Context window is nearly full. Consider using /compact to free space."
                .to_string(),
            severity: SuggestionSeverity::Critical,
        });
    } else if percentage >= 70.0 {
        suggestions.push(ContextSuggestion {
            message: "Context window is getting full. Consider using /compact soon.".to_string(),
            severity: SuggestionSeverity::Warning,
        });
    }

    // Check for large message categories (>50% of total usage)
    for cat in categories {
        if cat.name.starts_with("Messages") && cat.percentage > 50.0 {
            suggestions.push(ContextSuggestion {
                message: "Messages use over half the context. Use /compact to summarize."
                    .to_string(),
                severity: SuggestionSeverity::Warning,
            });
        }
    }

    if suggestions.is_empty() {
        suggestions.push(ContextSuggestion {
            message: "Context usage is healthy.".to_string(),
            severity: SuggestionSeverity::Info,
        });
    }

    suggestions
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

/// Format context data as a text table with a progress bar.
pub fn format_context_table(data: &ContextData) -> String {
    let mut lines = Vec::new();

    // Header
    lines.push(format!("Context Window Usage ({})", data.model));
    lines.push("━".repeat(50));
    lines.push(String::new());

    // If SDK tracking not available, note that estimates are approximate
    let approx = if data.api_token_counts.is_some() {
        ""
    } else {
        "~"
    };

    // Category table
    lines.push(format!(
        " {:<20} {:>10}   {:>5}",
        "Category", "Tokens", "%"
    ));
    lines.push(format!(" {}", "─".repeat(40)));

    for cat in &data.categories {
        if cat.name == "Available" {
            continue;
        }
        lines.push(format!(
            " {:<20} {:>9}{} {:>5.1}%",
            cat.name,
            format_number(cat.tokens),
            approx,
            cat.percentage,
        ));
    }

    lines.push(format!(" {}", "─".repeat(40)));

    // Totals
    lines.push(format!(
        " {:<20} {:>9}{} {:>5.1}%",
        "Total used",
        format_number(data.total_tokens),
        approx,
        data.percentage,
    ));

    let free = data.max_tokens.saturating_sub(data.total_tokens);
    lines.push(format!(
        " {:<20} {:>10}  {:>5.1}%",
        "Available",
        format_number(free),
        100.0 - data.percentage,
    ));
    lines.push(format!(
        " {:<20} {:>10}",
        "Max",
        format_number(data.max_tokens)
    ));

    lines.push(String::new());

    // Progress bar
    let bar_width: usize = 40;
    let filled = ((data.percentage / 100.0) * bar_width as f64).round() as usize;
    let empty = bar_width.saturating_sub(filled);
    let bar = format!(
        " [{}{}] {:.1}%",
        "█".repeat(filled),
        "░".repeat(empty),
        data.percentage,
    );
    lines.push(bar);
    lines.push(String::new());

    // Memory files detail
    if !data.memory_files.is_empty() {
        lines.push(" Memory files:".to_string());
        for mf in &data.memory_files {
            let display_path = shorten_path(&mf.path);
            lines.push(format!(
                "   {} ({})  {}{} tokens",
                display_path, mf.source_type, approx, format_number(mf.tokens)
            ));
        }
        lines.push(String::new());
    }

    // MCP tools detail
    if !data.mcp_tools.is_empty() {
        lines.push(" MCP tools:".to_string());
        for tool in &data.mcp_tools {
            lines.push(format!(
                "   {} ({})  {}{} tokens",
                tool.name, tool.server_name, approx, format_number(tool.tokens)
            ));
        }
        lines.push(String::new());
    }

    // Skills detail
    if !data.skills.is_empty() {
        lines.push(" Skills:".to_string());
        for skill in &data.skills {
            lines.push(format!(
                "   /{} ({})  {}{} tokens",
                skill.name, skill.source, approx, format_number(skill.tokens)
            ));
        }
        lines.push(String::new());
    }

    // Suggestions
    if !data.suggestions.is_empty() {
        lines.push(" Suggestions:".to_string());
        for s in &data.suggestions {
            let icon = match s.severity {
                SuggestionSeverity::Info => "✓",
                SuggestionSeverity::Warning => "⚠",
                SuggestionSeverity::Critical => "✗",
            };
            lines.push(format!("   {} {}", icon, s.message));
        }
    }

    lines.join("\n")
}

/// Format a number with comma separators.
fn format_number(n: u64) -> String {
    let s = n.to_string();
    let mut result = String::new();
    for (i, ch) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(ch);
    }
    result.chars().rev().collect()
}

/// Shorten a file path for display — replace home dir with ~.
fn shorten_path(path: &str) -> String {
    if let Some(home) = std::env::var_os("HOME") {
        let home_str = home.to_string_lossy();
        if path.starts_with(home_str.as_ref()) {
            return format!("~{}", &path[home_str.len()..]);
        }
    }
    // Shorten relative to cwd
    if let Ok(cwd) = std::env::current_dir() {
        let cwd_str = cwd.display().to_string();
        if path.starts_with(&cwd_str) {
            return format!(".{}", &path[cwd_str.len()..]);
        }
    }
    path.to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_tokens_basic() {
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn estimate_tokens_hello() {
        // "hello" = 5 chars, (5+3)/4 = 2
        assert_eq!(estimate_tokens("hello"), 2);
    }

    #[test]
    fn format_number_basic() {
        assert_eq!(format_number(0), "0");
        assert_eq!(format_number(999), "999");
        assert_eq!(format_number(1000), "1,000");
        assert_eq!(format_number(1_000_000), "1,000,000");
        assert_eq!(format_number(200_000), "200,000");
    }

    #[test]
    fn analyze_empty_context() {
        let input = AnalysisInput {
            model_name: "test-model".into(),
            system_prompt: String::new(),
            tool_specs: vec![],
            mcp_tool_specs: vec![],
            memory_files: vec![],
            skills: vec![],
            messages_json: vec![],
            sdk_token_counts: Some((0, 200_000)),
            sdk_context_percent: Some(0.0),
        };
        let data = analyze_context_usage(&input);
        assert_eq!(data.total_tokens, 0);
        assert_eq!(data.max_tokens, 200_000);
        assert!(data.percentage < 1.0);
    }

    #[test]
    fn analyze_with_messages() {
        let input = AnalysisInput {
            model_name: "test-model".into(),
            system_prompt: "You are a helpful assistant.".into(),
            tool_specs: vec![ToolSpecSummary {
                name: "Bash".into(),
                description: "Run shell commands".into(),
                input_schema_json: r#"{"type":"object","properties":{"command":{"type":"string"}}}"#.into(),
            }],
            mcp_tool_specs: vec![],
            memory_files: vec![],
            skills: vec![],
            messages_json: vec![serde_json::json!({"role": "user", "content": "hello world"})],
            sdk_token_counts: None,
            sdk_context_percent: None,
        };
        let data = analyze_context_usage(&input);
        assert!(data.total_tokens > 0);
        assert!(data.categories.len() >= 3); // system prompt + tools + messages + available
    }

    #[test]
    fn format_produces_output() {
        let data = ContextData {
            categories: vec![
                ContextCategory {
                    name: "System prompt".into(),
                    tokens: 4000,
                    percentage: 2.0,
                },
                ContextCategory {
                    name: "Messages (5)".into(),
                    tokens: 10000,
                    percentage: 5.0,
                },
            ],
            total_tokens: 14000,
            max_tokens: 200_000,
            percentage: 7.0,
            model: "test-model".into(),
            memory_files: vec![],
            mcp_tools: vec![],
            skills: vec![],
            suggestions: vec![ContextSuggestion {
                message: "Context usage is healthy.".into(),
                severity: SuggestionSeverity::Info,
            }],
            api_token_counts: None,
        };
        let output = format_context_table(&data);
        assert!(output.contains("Context Window Usage"));
        assert!(output.contains("System prompt"));
        assert!(output.contains("Messages (5)"));
        assert!(output.contains("healthy"));
    }
}
