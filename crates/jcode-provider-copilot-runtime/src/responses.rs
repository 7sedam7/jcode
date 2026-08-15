//! The Copilot `/responses` route.
//!
//! Eight models — the whole `gpt-5.5`/`gpt-5.6` family, `gpt-5.3-codex`,
//! `gpt-5.4-mini`, `grok-4.5` and `mai-code-1-flash-picker` — are served only
//! over the OpenAI Responses API. Sending them to `/chat/completions` returns
//! HTTP 400 `unsupported_api_for_model`, so without this route they are
//! unreachable.
//!
//! The request is built with the same pure builders the OpenAI provider uses,
//! with the deviations Copilot's implementation requires.

use jcode_message_types::{Message as ChatMessage, ToolDefinition};
use jcode_provider_openai::{build_responses_input, build_tools};
use serde_json::{Value, json};

/// Build a `/responses` request body.
pub fn build_request(
    model: &str,
    system: &str,
    messages: &[ChatMessage],
    tools: &[ToolDefinition],
    max_output_tokens: u32,
    reasoning_effort: Option<&str>,
) -> Value {
    let mut request = json!({
        "model": model,
        "instructions": system,
        "input": build_responses_input(messages),
        "stream": true,
        // Copilot rejects `store: true` outright ("store is not supported").
        // `false` is accepted and is also what we want: no server-side
        // retention of conversation state.
        "store": false,
        "max_output_tokens": max_output_tokens,
    });

    // Only advertise tools when there are some. grok-4.5 rejects the request
    // outright ("A tool_choice was set on the request but no tools were
    // specified") if `tool_choice` accompanies an empty `tools` array.
    let api_tools = build_tools(tools);
    if !api_tools.is_empty() {
        request["tools"] = json!(api_tools);
        request["tool_choice"] = json!("auto");
    }

    if let Some(effort) = reasoning_effort {
        request["reasoning"] = json!({ "effort": effort });
    }

    request
}

#[cfg(test)]
mod tests {
    use super::*;
    use jcode_message_types::{ContentBlock, Role};

    fn user(text: &str) -> ChatMessage {
        ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: text.to_string(),
                cache_control: None,
            }],
            timestamp: None,
            tool_duration_ms: None,
        }
    }

    #[test]
    fn store_is_never_true() {
        let body = build_request("gpt-5.5", "sys", &[user("hi")], &[], 1024, None);
        // Measured: `store: true` -> HTTP 400 "store is not supported".
        assert_eq!(body["store"], false);
    }

    #[test]
    fn system_prompt_becomes_instructions_not_a_message() {
        let body = build_request("gpt-5.5", "be terse", &[user("hi")], &[], 1024, None);
        assert_eq!(body["instructions"], "be terse");
        let input = body["input"].as_array().unwrap();
        assert!(input.iter().all(|m| m["role"] != "system"));
    }

    #[test]
    fn reasoning_effort_is_nested_under_reasoning() {
        let body = build_request("gpt-5.5", "", &[user("hi")], &[], 1024, Some("high"));
        assert_eq!(body["reasoning"]["effort"], "high");
        // The Chat Completions spelling must not leak onto this route.
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn output_cap_is_the_responses_spelling() {
        let body = build_request("gpt-5.5", "", &[user("hi")], &[], 4096, None);
        assert_eq!(body["max_output_tokens"], 4096);
        assert!(body.get("max_tokens").is_none());
    }

    #[test]
    fn tool_choice_is_omitted_when_there_are_no_tools() {
        let body = build_request("grok-4.5", "", &[user("hi")], &[], 1024, None);
        // grok-4.5 returns HTTP 400 "A tool_choice was set on the request but
        // no tools were specified" if either key is present without tools.
        assert!(body.get("tool_choice").is_none());
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn tools_use_the_flat_responses_schema() {
        let tools = vec![ToolDefinition {
            name: "get_weather".to_string(),
            description: "Weather".to_string(),
            input_schema: json!({"type": "object", "properties": {}}),
        }];
        let body = build_request("gpt-5.5", "", &[user("hi")], &tools, 1024, None);
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools[0]["type"], "function");
        // Responses flattens the function definition; it is not nested under
        // a "function" key as in Chat Completions.
        assert_eq!(tools[0]["name"], "get_weather");
    }
}
