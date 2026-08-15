//! Opt-in live check that jcode's `/v1/messages` request is accepted by the
//! real Copilot API and streams back Anthropic-shaped SSE.
//!
//! Ignored by default; needs a GitHub OAuth token in `COPILOT_LIVE_TOKEN`.

use futures::StreamExt;

#[tokio::test]
#[ignore = "hits the live Copilot API"]
async fn messages_route_is_accepted_and_streams() {
    let token = std::env::var("COPILOT_LIVE_TOKEN").expect("COPILOT_LIVE_TOKEN");
    let model = "claude-sonnet-4.6";

    let body = jcode_provider_copilot_runtime::testing::build_messages_request(
        model,
        "You are terse.",
        "Reply with exactly: PONG",
        512,
    );

    let resp = reqwest::Client::new()
        .post(format!(
            "{}/v1/messages",
            jcode_base::auth::copilot::COPILOT_API_BASE
        ))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .header("X-Initiator", "user")
        .header("X-GitHub-Api-Version", "2025-04-01")
        .header("Editor-Version", jcode_base::auth::copilot::EDITOR_VERSION)
        .header(
            "anthropic-beta",
            jcode_provider_copilot_runtime::testing::ANTHROPIC_BETA,
        )
        .json(&body)
        .send()
        .await
        .expect("request sent");

    let status = resp.status();
    assert!(
        status.is_success(),
        "HTTP {status}: {}",
        resp.text().await.unwrap_or_default()
    );

    let mut stream = resp.bytes_stream();
    let mut transcript = String::new();
    while let Some(chunk) = stream.next().await {
        transcript.push_str(&String::from_utf8_lossy(&chunk.expect("chunk")));
    }

    assert!(
        transcript.contains("message_start"),
        "no message_start: {transcript:.400}"
    );
    assert!(
        transcript.contains("content_block_delta"),
        "no content deltas: {transcript:.400}"
    );
    assert!(transcript.contains("PONG"), "no answer: {transcript:.800}");
    println!("live /v1/messages OK ({} bytes of SSE)", transcript.len());
}

/// The full provider path: catalog fetch -> route selection -> Anthropic-shaped
/// request -> Anthropic SSE decode -> `StreamEvent`s.
#[tokio::test]
#[ignore = "hits the live Copilot API"]
async fn provider_streams_a_claude_turn_end_to_end() {
    use jcode_message_types::{ContentBlock, Message, Role, StreamEvent};
    use jcode_provider_core::Provider;

    let token = std::env::var("COPILOT_LIVE_TOKEN").expect("COPILOT_LIVE_TOKEN");
    let provider =
        jcode_provider_copilot_runtime::CopilotApiProvider::new_with_token(token.clone());
    let _ = provider.set_model("claude-sonnet-4.6");
    provider.detect_tier_and_set_default().await;
    // Tier detection may switch the default model; pin the one under test.
    let _ = provider.set_model("claude-sonnet-4.6");

    let messages = vec![Message {
        role: Role::User,
        content: vec![ContentBlock::Text {
            text: "Reply with exactly: PONG".to_string(),
            cache_control: None,
        }],
        timestamp: None,
        tool_duration_ms: None,
    }];

    let mut stream = provider
        .complete(&messages, &[], "You are terse.", None)
        .await
        .expect("stream opened");

    let mut text = String::new();
    let mut saw_end = false;
    while let Some(event) = stream.next().await {
        match event.expect("no stream error") {
            StreamEvent::TextDelta(t) => text.push_str(&t),
            StreamEvent::MessageEnd { .. } => saw_end = true,
            _ => {}
        }
    }

    assert!(text.contains("PONG"), "unexpected reply: {text:?}");
    assert!(saw_end, "stream never produced MessageEnd");
    println!("live end-to-end OK: {text:?}");
}

/// Tool calls are the part of the turn the old chat shim flattened, so verify
/// the Messages route round-trips a real tool_use block.
#[tokio::test]
#[ignore = "hits the live Copilot API"]
async fn provider_emits_tool_calls_on_the_messages_route() {
    use jcode_message_types::{ContentBlock, Message, Role, StreamEvent, ToolDefinition};
    use jcode_provider_core::Provider;

    let token = std::env::var("COPILOT_LIVE_TOKEN").expect("COPILOT_LIVE_TOKEN");
    let provider = jcode_provider_copilot_runtime::CopilotApiProvider::new_with_token(token);
    provider.detect_tier_and_set_default().await;
    let _ = provider.set_model("claude-sonnet-4.6");

    let tools = vec![ToolDefinition {
        name: "get_weather".to_string(),
        description: "Get the current weather for a city".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {"city": {"type": "string"}},
            "required": ["city"]
        }),
    }];

    let messages = vec![Message {
        role: Role::User,
        content: vec![ContentBlock::Text {
            text: "What is the weather in Paris? Use the tool.".to_string(),
            cache_control: None,
        }],
        timestamp: None,
        tool_duration_ms: None,
    }];

    let mut stream = provider
        .complete(&messages, &tools, "Use tools when asked.", None)
        .await
        .expect("stream opened");

    let mut tool_name = None;
    let mut tool_args = String::new();
    while let Some(event) = stream.next().await {
        match event.expect("no stream error") {
            StreamEvent::ToolUseStart { name, .. } => tool_name = Some(name),
            StreamEvent::ToolInputDelta(delta) => tool_args.push_str(&delta),
            _ => {}
        }
    }

    assert_eq!(tool_name.as_deref(), Some("get_weather"));
    assert!(
        tool_args.to_lowercase().contains("paris"),
        "tool args lost the argument: {tool_args:?}"
    );
    println!("live tool call OK: {tool_args}");
}
