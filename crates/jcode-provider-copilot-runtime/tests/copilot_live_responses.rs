//! Opt-in live check for the `/responses` route.
//!
//! These models return HTTP 400 `unsupported_api_for_model` on
//! `/chat/completions`, so before routing existed they could not be used at all.
//! Needs a GitHub OAuth token in `COPILOT_LIVE_TOKEN`.

use futures::StreamExt;
use jcode_message_types::{ContentBlock, Message, Role, StreamEvent, ToolDefinition};
use jcode_provider_core::Provider;

fn user(text: &str) -> Message {
    Message {
        role: Role::User,
        content: vec![ContentBlock::Text {
            text: text.to_string(),
            cache_control: None,
        }],
        timestamp: None,
        tool_duration_ms: None,
    }
}

async fn provider(model: &str) -> jcode_provider_copilot_runtime::CopilotApiProvider {
    let token = std::env::var("COPILOT_LIVE_TOKEN").expect("COPILOT_LIVE_TOKEN");
    let p = jcode_provider_copilot_runtime::CopilotApiProvider::new_with_token(token);
    p.detect_tier_and_set_default().await;
    let _ = p.set_model(model);
    p
}

#[tokio::test]
#[ignore = "hits the live Copilot API"]
async fn every_responses_only_model_streams() {
    for model in [
        "gpt-5.5",
        "gpt-5.6-sol",
        "gpt-5.6-luna",
        "gpt-5.6-terra",
        "gpt-5.3-codex",
        "gpt-5.4-mini",
        "grok-4.5",
        "mai-code-1-flash-picker",
    ] {
        let p = provider(model).await;
        let mut stream = p
            .complete(
                &[user("Reply with exactly: PONG")],
                &[],
                "You are terse.",
                None,
            )
            .await
            .unwrap_or_else(|e| panic!("{model}: stream did not open: {e}"));

        let mut text = String::new();
        while let Some(event) = stream.next().await {
            if let StreamEvent::TextDelta(t) =
                event.unwrap_or_else(|e| panic!("{model}: stream error: {e}"))
            {
                text.push_str(&t);
            }
        }
        assert!(
            text.to_uppercase().contains("PONG"),
            "{model}: unexpected reply {text:?}"
        );
        println!("{model}: OK ({text:?})");
    }
}

#[tokio::test]
#[ignore = "hits the live Copilot API"]
async fn responses_route_emits_tool_calls() {
    let p = provider("gpt-5.5").await;
    let tools = vec![ToolDefinition {
        name: "get_weather".to_string(),
        description: "Get the current weather for a city".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {"city": {"type": "string"}},
            "required": ["city"]
        }),
    }];

    let mut stream = p
        .complete(
            &[user("What is the weather in Paris? Use the tool.")],
            &tools,
            "Use tools when asked.",
            None,
        )
        .await
        .expect("stream opened");

    let mut name = None;
    let mut args = String::new();
    while let Some(event) = stream.next().await {
        match event.expect("stream error") {
            StreamEvent::ToolUseStart { name: n, .. } => name = Some(n),
            StreamEvent::ToolInputDelta(d) => args.push_str(&d),
            _ => {}
        }
    }

    assert_eq!(name.as_deref(), Some("get_weather"));
    assert!(
        args.to_lowercase().contains("paris"),
        "tool args lost the argument: {args:?}"
    );
    println!("responses tool call OK: {args}");
}

/// Replaying a completed tool call is what the old chat shim flattened, and
/// what a long agent session does on every turn. Verify both routes accept a
/// history containing assistant tool_use + user tool_result.
#[tokio::test]
#[ignore = "hits the live Copilot API"]
async fn both_routes_accept_replayed_tool_history() {
    let tools = vec![ToolDefinition {
        name: "get_weather".to_string(),
        description: "Get the current weather for a city".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {"city": {"type": "string"}},
            "required": ["city"]
        }),
    }];

    for model in ["claude-sonnet-4.6", "gpt-5.5"] {
        let history = vec![
            user("What is the weather in Paris? Use the tool."),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "toolu_01replay".to_string(),
                    name: "get_weather".to_string(),
                    input: serde_json::json!({"city": "Paris"}),
                    thought_signature: None,
                }],
                timestamp: None,
                tool_duration_ms: None,
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "toolu_01replay".to_string(),
                    content: "18C and sunny".to_string(),
                    is_error: None,
                }],
                timestamp: None,
                tool_duration_ms: None,
            },
        ];

        let p = provider(model).await;
        let mut stream = p
            .complete(&history, &tools, "Use tools when asked.", None)
            .await
            .unwrap_or_else(|e| panic!("{model}: stream did not open: {e}"));

        let mut text = String::new();
        while let Some(event) = stream.next().await {
            if let StreamEvent::TextDelta(t) =
                event.unwrap_or_else(|e| panic!("{model}: stream error: {e}"))
            {
                text.push_str(&t);
            }
        }

        // The model must have seen the tool result to answer at all.
        assert!(
            text.contains("18") || text.to_lowercase().contains("sunny"),
            "{model}: tool result did not reach the model: {text:?}"
        );
        println!("{model}: tool replay OK ({text:?})");
    }
}
