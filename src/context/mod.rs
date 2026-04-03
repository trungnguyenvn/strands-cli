//! Context gathering: git status, STRANDS.md discovery.

pub mod git;
pub mod user_context;

pub use git::{get_git_status, GitContext};
pub use user_context::get_user_context;
