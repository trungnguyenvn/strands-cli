//! PromptSection trait and render context.

use crate::context::GitContext;

/// Everything a prompt section needs to render itself.
pub struct RenderContext<'a> {
    pub tool_names: &'a [String],
    pub cwd: &'a str,
    pub platform: &'a str,
    pub shell: &'a str,
    pub git: Option<&'a GitContext>,
    pub date: &'a str,
    pub has_user_context: bool,
}

/// A composable section of the system prompt.
pub trait PromptSection: Send + Sync {
    #[allow(dead_code)]
    fn name(&self) -> &'static str;
    fn render(&self, ctx: &RenderContext<'_>) -> Option<String>;
}
