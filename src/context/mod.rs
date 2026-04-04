//! Context gathering: git status, STRANDS.md discovery, context analysis.

pub mod analysis;
pub mod git;
pub mod user_context;

pub use analysis::{
    analyze_context_usage, format_context_table, AnalysisInput, SkillSummary, ToolSpecSummary,
};
pub use git::{get_git_status, GitContext};
pub use user_context::get_user_context;
