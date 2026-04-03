//! PromptBuilder — assembles prompt sections into a final string.

use super::section::{PromptSection, RenderContext};
use super::static_sections::*;
use super::dynamic_sections::*;

/// Collects prompt sections and renders them to a single string.
pub struct PromptBuilder {
    sections: Vec<Box<dyn PromptSection>>,
}

impl PromptBuilder {
    /// Create a builder with the default set of sections in standard order.
    pub fn with_defaults() -> Self {
        Self {
            sections: vec![
                Box::new(IntroSection),
                Box::new(SystemSection),
                Box::new(DoingTasksSection),
                Box::new(ActionsSection),
                Box::new(UsingToolsSection),
                Box::new(ToneSection),
                Box::new(OutputEfficiencySection),
                Box::new(EnvInfoSection),
                Box::new(SessionGuidanceSection),
            ],
        }
    }

    /// Render all sections into a single prompt string.
    pub fn build(&self, ctx: &RenderContext<'_>) -> String {
        self.sections
            .iter()
            .filter_map(|s| s.render(ctx))
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}
