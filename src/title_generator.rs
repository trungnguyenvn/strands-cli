//! AI-powered session title generation.
//!
//! Mirrors Claude Code's `generateSessionTitle()` from `src/utils/sessionTitle.ts`.
//! Makes a single non-streaming model call to generate a short, descriptive title
//! for the conversation.

use std::sync::Arc;
use strands::types::content::Message;
use strands::types::models::{Model, ModelConfig, ModelRequest};

const TITLE_SYSTEM_PROMPT: &str = "\
You are a helpful assistant that generates concise session titles.
Generate a short title (3-7 words) in sentence case that summarizes the conversation topic.
Return ONLY a JSON object with a single \"title\" field, like: {\"title\": \"Fix login page bug\"}
Do not include any other text or explanation.";

/// Generate a session title from the conversation text using the given model.
///
/// Returns `None` on any error (timeout, parse failure, model error).
/// This function is designed to be called from a `tokio::spawn` background task.
pub async fn generate_session_title(
    conversation_text: &str,
    model: Arc<dyn Model>,
) -> Option<String> {
    // Truncate input to avoid using too many tokens
    let truncated: String = conversation_text.chars().take(500).collect();

    let request = ModelRequest {
        messages: vec![Message::user(truncated)],
        system_prompt: Some(TITLE_SYSTEM_PROMPT.to_string()),
        tools: Vec::new(),
        config: ModelConfig {
            temperature: Some(0.3),
            max_tokens: Some(100),
            top_p: None,
            top_k: None,
            stop_sequences: None,
            streaming: false,
        },
    };

    // Use a timeout to avoid blocking if the model is slow
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        model.converse(&request),
    )
    .await;

    let response = match result {
        Ok(Ok(resp)) => resp,
        _ => return None,
    };

    // Extract text from the response
    let text = match response {
        strands::types::models::ModelResponse::Text(t) => t,
        strands::types::models::ModelResponse::Mixed { text: Some(t), .. } => t,
        _ => return None,
    };

    // Parse the JSON response to extract the title field
    parse_title_from_response(&text)
}

/// Parse a title from the model's JSON response.
///
/// Handles both clean JSON and JSON embedded in markdown code blocks.
fn parse_title_from_response(text: &str) -> Option<String> {
    let trimmed = text.trim();

    // Try direct JSON parse
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
        if let Some(title) = v.get("title").and_then(|t| t.as_str()) {
            let title = title.trim();
            if !title.is_empty() {
                return Some(title.to_string());
            }
        }
    }

    // Try extracting JSON from markdown code block
    if let Some(start) = trimmed.find('{') {
        if let Some(end) = trimmed.rfind('}') {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&trimmed[start..=end]) {
                if let Some(title) = v.get("title").and_then(|t| t.as_str()) {
                    let title = title.trim();
                    if !title.is_empty() {
                        return Some(title.to_string());
                    }
                }
            }
        }
    }

    // Last resort: if the response is a short plain string, use it directly
    if trimmed.len() <= 60 && !trimmed.contains('\n') && !trimmed.starts_with('{') {
        return Some(trimmed.to_string());
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_clean_json() {
        let input = r#"{"title": "Fix login page bug"}"#;
        assert_eq!(
            parse_title_from_response(input),
            Some("Fix login page bug".to_string())
        );
    }

    #[test]
    fn parse_json_in_code_block() {
        let input = "```json\n{\"title\": \"Add user auth\"}\n```";
        assert_eq!(
            parse_title_from_response(input),
            Some("Add user auth".to_string())
        );
    }

    #[test]
    fn parse_plain_text_fallback() {
        let input = "Fix login page bug";
        assert_eq!(
            parse_title_from_response(input),
            Some("Fix login page bug".to_string())
        );
    }

    #[test]
    fn parse_empty_title_returns_none() {
        let input = r#"{"title": ""}"#;
        assert_eq!(parse_title_from_response(input), None);
    }
}
