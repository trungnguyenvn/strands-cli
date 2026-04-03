//! Modular system prompt building.

pub mod section;
pub mod static_sections;
pub mod dynamic_sections;
pub mod builder;

pub use section::RenderContext;
use builder::PromptBuilder;

/// Where the system prompt comes from.
pub enum PromptSource {
    /// CLI `--system` flag — use as-is.
    Override(String),
    /// Built-in default sections.
    Default,
}

/// Build the effective system prompt based on the source and render context.
pub fn build_effective_system_prompt(
    source: PromptSource,
    ctx: &RenderContext<'_>,
) -> String {
    match source {
        PromptSource::Override(s) => s,
        PromptSource::Default => PromptBuilder::with_defaults().build(ctx),
    }
}
