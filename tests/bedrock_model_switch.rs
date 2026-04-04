//! E2E integration tests for Bedrock model switching.
//!
//! These tests call the real AWS Bedrock API using the EC2 instance IAM role.
//! They verify that:
//! 1. `build_model_by_id` correctly constructs each provider's model
//! 2. The constructed model can be swapped into an Agent via `swap_model`
//! 3. The swapped model actually responds (single-turn "say hi")
//!
//! Run with: AWS_PROFILE= cargo test --test bedrock_model_switch -- --nocapture
//! (Unset AWS_PROFILE to use the IAM role instead of a named profile.)

use std::sync::Arc;

use strands::models::bedrock::{BedrockConfig, BedrockModel};
use strands::Agent;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a BedrockModel for the given inference profile ID.
async fn make_bedrock_model(model_id: &str) -> Arc<dyn strands::types::models::Model> {
    let mut config = BedrockConfig::default();
    config.model_id = model_id.to_string();
    config.max_tokens = Some(256);
    // Use us-east-1 for broadest profile availability; pass as region_name (3rd arg)
    let model = BedrockModel::new(None, None, Some("us-east-1".to_string()), config)
        .await
        .unwrap_or_else(|e| panic!("Failed to create model {}: {}", model_id, e));
    Arc::new(model)
}

/// Build a minimal Agent with no tools and a short system prompt.
async fn make_test_agent(model_id: &str) -> Agent {
    let model = make_bedrock_model(model_id).await;
    Agent::builder()
        .with_model(model)
        .with_system_prompt("You are a test assistant. Reply in 10 words or fewer.")
        .with_max_iterations(1)
        .build()
        .await
        .expect("Failed to build agent")
}

/// Execute a single prompt and return the text response.
async fn single_turn(agent: &Agent, prompt: &str) -> String {
    let result = agent.execute(prompt).await
        .unwrap_or_else(|e| panic!("Agent execution failed: {}", e));
    result.text()
}

// ---------------------------------------------------------------------------
// Tests: Build each Bedrock model (verifies IAM + model ID validity)
// ---------------------------------------------------------------------------

/// Claude Sonnet 4.6
#[tokio::test]
async fn build_bedrock_claude_sonnet_46() {
    let _model = make_bedrock_model("us.anthropic.claude-sonnet-4-6").await;
}

/// Claude Opus 4.6
#[tokio::test]
async fn build_bedrock_claude_opus_46() {
    let _model = make_bedrock_model("us.anthropic.claude-opus-4-6-v1").await;
}

/// Claude Haiku 4.5
#[tokio::test]
async fn build_bedrock_claude_haiku_45() {
    let _model = make_bedrock_model("us.anthropic.claude-haiku-4-5-20251001-v1:0").await;
}

/// Claude Sonnet 4
#[tokio::test]
async fn build_bedrock_claude_sonnet_4() {
    let _model = make_bedrock_model("us.anthropic.claude-sonnet-4-20250514-v1:0").await;
}

/// Claude Opus 4
#[tokio::test]
async fn build_bedrock_claude_opus_4() {
    let _model = make_bedrock_model("us.anthropic.claude-opus-4-20250514-v1:0").await;
}

/// Claude Sonnet 4.5
#[tokio::test]
async fn build_bedrock_claude_sonnet_45() {
    let _model = make_bedrock_model("us.anthropic.claude-sonnet-4-5-20250929-v1:0").await;
}

/// Amazon Nova Pro
#[tokio::test]
async fn build_bedrock_nova_pro() {
    let _model = make_bedrock_model("us.amazon.nova-pro-v1:0").await;
}

/// Amazon Nova Lite
#[tokio::test]
async fn build_bedrock_nova_lite() {
    let _model = make_bedrock_model("us.amazon.nova-lite-v1:0").await;
}

/// Amazon Nova Micro
#[tokio::test]
async fn build_bedrock_nova_micro() {
    let _model = make_bedrock_model("us.amazon.nova-micro-v1:0").await;
}

/// Amazon Nova Premier
#[tokio::test]
async fn build_bedrock_nova_premier() {
    let _model = make_bedrock_model("us.amazon.nova-premier-v1:0").await;
}

/// Meta Llama 4 Scout
#[tokio::test]
async fn build_bedrock_llama4_scout() {
    let _model = make_bedrock_model("us.meta.llama4-scout-17b-instruct-v1:0").await;
}

/// Meta Llama 4 Maverick
#[tokio::test]
async fn build_bedrock_llama4_maverick() {
    let _model = make_bedrock_model("us.meta.llama4-maverick-17b-instruct-v1:0").await;
}

/// Meta Llama 3.3 70B
#[tokio::test]
async fn build_bedrock_llama33_70b() {
    let _model = make_bedrock_model("us.meta.llama3-3-70b-instruct-v1:0").await;
}

/// Mistral Pixtral Large
#[tokio::test]
async fn build_bedrock_mistral_pixtral() {
    let _model = make_bedrock_model("us.mistral.pixtral-large-2502-v1:0").await;
}

// ---------------------------------------------------------------------------
// Tests: Single-turn conversation — send simple message, validate response
// ---------------------------------------------------------------------------

/// Claude Sonnet 4.6 — math question
#[tokio::test]
async fn chat_claude_sonnet_46() {
    let agent = make_test_agent("us.anthropic.claude-sonnet-4-6").await;
    let response = single_turn(&agent, "What is 2+3? Reply with just the number.").await;
    let lower = response.to_lowercase();
    assert!(lower.contains('5'), "Sonnet 4.6 should answer 5, got: {}", response);
    println!("Sonnet 4.6: {}", response);
}

/// Claude Opus 4.6 — math question
#[tokio::test]
async fn chat_claude_opus_46() {
    let agent = make_test_agent("us.anthropic.claude-opus-4-6-v1").await;
    let response = single_turn(&agent, "What is 2+3? Reply with just the number.").await;
    let lower = response.to_lowercase();
    assert!(lower.contains('5'), "Opus 4.6 should answer 5, got: {}", response);
    println!("Opus 4.6: {}", response);
}

/// Claude Haiku 4.5 — capital city question
#[tokio::test]
async fn chat_claude_haiku_45() {
    let agent = make_test_agent("us.anthropic.claude-haiku-4-5-20251001-v1:0").await;
    let response = single_turn(&agent, "What is the capital of France? One word.").await;
    let lower = response.to_lowercase();
    assert!(lower.contains("paris"), "Haiku 4.5 should say Paris, got: {}", response);
    println!("Haiku 4.5: {}", response);
}

/// Claude Sonnet 4 — color question
#[tokio::test]
async fn chat_claude_sonnet_4() {
    let agent = make_test_agent("us.anthropic.claude-sonnet-4-20250514-v1:0").await;
    let response = single_turn(&agent, "What color is the sky on a clear day? One word.").await;
    let lower = response.to_lowercase();
    assert!(lower.contains("blue"), "Sonnet 4 should say blue, got: {}", response);
    println!("Sonnet 4: {}", response);
}

/// Claude Sonnet 4.5 — math
#[tokio::test]
async fn chat_claude_sonnet_45() {
    let agent = make_test_agent("us.anthropic.claude-sonnet-4-5-20250929-v1:0").await;
    let response = single_turn(&agent, "What is 10-3? Reply with just the number.").await;
    let lower = response.to_lowercase();
    assert!(lower.contains('7') || lower.contains("seven"), "Sonnet 4.5 should answer 7, got: {}", response);
    println!("Sonnet 4.5: {}", response);
}

/// Amazon Nova Micro — math
#[tokio::test]
async fn chat_nova_micro() {
    let agent = make_test_agent("us.amazon.nova-micro-v1:0").await;
    let response = single_turn(&agent, "What is 2+3? Reply with just the number.").await;
    let lower = response.to_lowercase();
    assert!(lower.contains('5') || lower.contains("five"), "Nova Micro should answer 5, got: {}", response);
    println!("Nova Micro: {}", response);
}

/// Amazon Nova Lite — capital city
#[tokio::test]
async fn chat_nova_lite() {
    let agent = make_test_agent("us.amazon.nova-lite-v1:0").await;
    let response = single_turn(&agent, "What is the capital of Japan? One word.").await;
    let lower = response.to_lowercase();
    assert!(lower.contains("tokyo"), "Nova Lite should say Tokyo, got: {}", response);
    println!("Nova Lite: {}", response);
}

/// Amazon Nova Pro — math
#[tokio::test]
async fn chat_nova_pro() {
    let agent = make_test_agent("us.amazon.nova-pro-v1:0").await;
    let response = single_turn(&agent, "What is 7*8? Reply with just the number.").await;
    let lower = response.to_lowercase();
    assert!(lower.contains("56") || lower.contains("fifty-six") || lower.contains("fifty six"),
        "Nova Pro should answer 56, got: {}", response);
    println!("Nova Pro: {}", response);
}

/// Meta Llama 4 Scout — math
#[tokio::test]
async fn chat_llama4_scout() {
    let agent = make_test_agent("us.meta.llama4-scout-17b-instruct-v1:0").await;
    let response = single_turn(&agent, "What is 2+3? Reply with just the number.").await;
    let lower = response.to_lowercase();
    assert!(lower.contains('5'), "Llama 4 Scout should answer 5, got: {}", response);
    println!("Llama 4 Scout: {}", response);
}

/// Meta Llama 4 Maverick — capital city
#[tokio::test]
async fn chat_llama4_maverick() {
    let agent = make_test_agent("us.meta.llama4-maverick-17b-instruct-v1:0").await;
    let response = single_turn(&agent, "What is the capital of Germany? One word.").await;
    let lower = response.to_lowercase();
    assert!(lower.contains("berlin"), "Llama 4 Maverick should say Berlin, got: {}", response);
    println!("Llama 4 Maverick: {}", response);
}

/// Meta Llama 3.3 70B — math
#[tokio::test]
async fn chat_llama33_70b() {
    let agent = make_test_agent("us.meta.llama3-3-70b-instruct-v1:0").await;
    let response = single_turn(&agent, "What is 9+1? Reply with just the number.").await;
    let lower = response.to_lowercase();
    assert!(lower.contains("10"), "Llama 3.3 70B should answer 10, got: {}", response);
    println!("Llama 3.3 70B: {}", response);
}

/// Mistral Pixtral Large — math
#[tokio::test]
async fn chat_mistral_pixtral() {
    let agent = make_test_agent("us.mistral.pixtral-large-2502-v1:0").await;
    let response = single_turn(&agent, "What is 4+6? Reply with just the number.").await;
    let lower = response.to_lowercase();
    assert!(lower.contains("10"), "Mistral should answer 10, got: {}", response);
    println!("Mistral Pixtral: {}", response);
}

// ---------------------------------------------------------------------------
// Tests: swap_model — switch between models, validate each responds correctly
// ---------------------------------------------------------------------------

/// Haiku → Nova Micro: each answers a different math question correctly.
#[tokio::test]
async fn swap_haiku_to_nova_micro() {
    let agent = make_test_agent("us.anthropic.claude-haiku-4-5-20251001-v1:0").await;

    let r1 = single_turn(&agent, "What is 3+4? Reply with just the number.").await;
    assert!(r1.contains('7'), "Haiku should answer 7, got: {}", r1);
    println!("Before swap (Haiku): {}", r1);

    let nova = make_bedrock_model("us.amazon.nova-micro-v1:0").await;
    agent.swap_model(nova);
    agent.clear_history();

    let r2 = single_turn(&agent, "What is 5+5? Reply with just the number.").await;
    assert!(r2.contains("10") || r2.to_lowercase().contains("ten"),
        "Nova Micro should answer 10 after swap, got: {}", r2);
    println!("After swap (Nova Micro): {}", r2);
}

/// Nova Micro → Sonnet 4.6: each answers a capital city question.
#[tokio::test]
async fn swap_nova_to_sonnet() {
    let agent = make_test_agent("us.amazon.nova-micro-v1:0").await;

    let r1 = single_turn(&agent, "What is the capital of Italy? One word.").await;
    let l1 = r1.to_lowercase();
    assert!(l1.contains("rome"), "Nova should say Rome, got: {}", r1);
    println!("Before swap (Nova): {}", r1);

    let sonnet = make_bedrock_model("us.anthropic.claude-sonnet-4-6").await;
    agent.swap_model(sonnet);
    agent.clear_history();

    let r2 = single_turn(&agent, "What is the capital of Spain? One word.").await;
    let l2 = r2.to_lowercase();
    assert!(l2.contains("madrid"), "Sonnet should say Madrid after swap, got: {}", r2);
    println!("After swap (Sonnet): {}", r2);
}

/// 3-way chain: Claude Haiku → Nova Micro → Llama 4 Scout.
/// Each answers a different simple question to prove the model actually changed.
#[tokio::test]
async fn swap_chain_claude_nova_llama() {
    let agent = make_test_agent("us.anthropic.claude-haiku-4-5-20251001-v1:0").await;

    // Claude Haiku: math
    let r1 = single_turn(&agent, "What is 2+2? Reply with just the number.").await;
    assert!(r1.contains('4'), "Claude should answer 4, got: {}", r1);
    println!("Step 1 (Claude Haiku): {}", r1);

    // Swap to Nova Micro: capital
    let nova = make_bedrock_model("us.amazon.nova-micro-v1:0").await;
    agent.swap_model(nova);
    agent.clear_history();

    let r2 = single_turn(&agent, "What is the capital of France? One word.").await;
    let l2 = r2.to_lowercase();
    assert!(l2.contains("paris"), "Nova should say Paris, got: {}", r2);
    println!("Step 2 (Nova Micro): {}", r2);

    // Swap to Llama 4 Scout: math
    let llama = make_bedrock_model("us.meta.llama4-scout-17b-instruct-v1:0").await;
    agent.swap_model(llama);
    agent.clear_history();

    let r3 = single_turn(&agent, "What is 8-3? Reply with just the number.").await;
    assert!(r3.contains('5'), "Llama should answer 5, got: {}", r3);
    println!("Step 3 (Llama 4 Scout): {}", r3);
}

/// 4-way chain: Sonnet 4.6 → Nova Lite → Llama 4 Maverick → Haiku 3.5.
#[tokio::test]
async fn swap_chain_four_providers() {
    let agent = make_test_agent("us.anthropic.claude-sonnet-4-6").await;

    let r1 = single_turn(&agent, "What is 1+1? Reply with just the number.").await;
    assert!(r1.contains('2'), "Sonnet 4.6 should answer 2, got: {}", r1);
    println!("Step 1 (Sonnet 4.6): {}", r1);

    // → Nova Lite
    let nova = make_bedrock_model("us.amazon.nova-lite-v1:0").await;
    agent.swap_model(nova);
    agent.clear_history();

    let r2 = single_turn(&agent, "What is 3*3? Reply with just the number.").await;
    assert!(r2.contains('9') || r2.to_lowercase().contains("nine"),
        "Nova Lite should answer 9, got: {}", r2);
    println!("Step 2 (Nova Lite): {}", r2);

    // → Llama 4 Maverick
    let llama = make_bedrock_model("us.meta.llama4-maverick-17b-instruct-v1:0").await;
    agent.swap_model(llama);
    agent.clear_history();

    let r3 = single_turn(&agent, "What is 6+6? Reply with just the number.").await;
    assert!(r3.contains("12"), "Llama Maverick should answer 12, got: {}", r3);
    println!("Step 3 (Llama 4 Maverick): {}", r3);

    // → Haiku 4.5
    let haiku = make_bedrock_model("us.anthropic.claude-haiku-4-5-20251001-v1:0").await;
    agent.swap_model(haiku);
    agent.clear_history();

    let r4 = single_turn(&agent, "What is 100-1? Reply with just the number.").await;
    assert!(r4.contains("99") || r4.to_lowercase().contains("ninety-nine"),
        "Haiku 4.5 should answer 99, got: {}", r4);
    println!("Step 4 (Haiku 4.5): {}", r4);
}
