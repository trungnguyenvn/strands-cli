//! Dynamic prompt sections — content that depends on runtime state.

use super::section::{PromptSection, RenderContext};

// ---------------------------------------------------------------------------
// Environment info
// ---------------------------------------------------------------------------

pub struct EnvInfoSection;

impl PromptSection for EnvInfoSection {
    fn name(&self) -> &'static str { "env_info" }

    fn render(&self, ctx: &RenderContext<'_>) -> Option<String> {
        let mut lines = vec![
            "# Environment".to_string(),
            format!(" - Working directory: {}", ctx.cwd),
            format!(" - Platform: {}", ctx.platform),
            format!(" - Shell: {}", ctx.shell),
            format!(" - Date: {}", ctx.date),
        ];

        if let Some(git) = ctx.git {
            lines.push(format!(" - Git branch: {}", git.branch));
            lines.push(format!(" - Main branch: {}", git.main_branch));
            if let Some(ref name) = git.user_name {
                lines.push(format!(" - Git user: {}", name));
            }
            lines.push(format!(" - Status:\n{}", git.status));
            lines.push(format!(" - Recent commits:\n{}", git.recent_commits));
        } else {
            lines.push(" - Git: not a git repository".to_string());
        }

        if !ctx.mcp_server_names.is_empty() {
            lines.push(format!(
                " - MCP servers: {}",
                ctx.mcp_server_names.join(", ")
            ));
        }

        Some(lines.join("\n"))
    }
}

// ---------------------------------------------------------------------------
// Skills listing
// ---------------------------------------------------------------------------

pub struct SkillsSection;

impl PromptSection for SkillsSection {
    fn name(&self) -> &'static str { "skills" }

    fn render(&self, ctx: &RenderContext<'_>) -> Option<String> {
        if ctx.skills.is_empty() {
            return None;
        }

        let mut lines = vec![
            "# Skills".to_string(),
            "Use the Skill tool to invoke skills, or type /skillname:".to_string(),
            String::new(),
        ];

        for skill in ctx.skills {
            let mut entry = format!("- {}: {}", skill.name, skill.description);
            if let Some(ref when) = skill.when_to_use {
                entry.push_str(&format!(" — {}", when));
            }
            lines.push(entry);
        }

        Some(lines.join("\n"))
    }
}

// ---------------------------------------------------------------------------
// Session guidance
// ---------------------------------------------------------------------------

pub struct SessionGuidanceSection;

impl PromptSection for SessionGuidanceSection {
    fn name(&self) -> &'static str { "session_guidance" }

    fn render(&self, ctx: &RenderContext<'_>) -> Option<String> {
        if !ctx.has_user_context {
            return None;
        }
        Some("# Session context\nProject context from STRANDS.md has been provided at the start of this conversation. Refer to it for project-specific conventions and instructions.".into())
    }
}
