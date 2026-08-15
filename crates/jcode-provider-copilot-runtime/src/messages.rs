//! The Copilot `/v1/messages` route.
//!
//! For Claude models Copilot serves Anthropic's native Messages API rather than
//! an OpenAI-shaped shim, so the request is built with the same pure
//! serializers the Anthropic provider uses and the response is decoded with the
//! same SSE decoder. Only transport, auth and a few Copilot deviations differ.

use jcode_message_types::{Message as ChatMessage, ToolDefinition};
use jcode_provider_anthropic::{ApiRequest, format_messages, format_tools};
use serde_json::Value;

/// Copilot's Messages route is not an OAuth-attributed Claude Code session, so
/// the Claude Code identity injection and tool-name mapping must stay off.
const IS_OAUTH: bool = false;

/// Copilot does not honour Anthropic's extended (1h) cache TTL.
const CACHE_TTL_1H: bool = false;

/// Interleaved thinking lets Claude emit reasoning between tool calls, which is
/// what keeps reasoning state coherent across a long tool-using session.
pub const ANTHROPIC_BETA: &str = "interleaved-thinking-2025-05-14";

/// Build a `/v1/messages` request body.
pub fn build_request(
    model: &str,
    system: &str,
    messages: &[ChatMessage],
    tools: &[ToolDefinition],
    max_tokens: u32,
    reasoning_effort: Option<&str>,
) -> Value {
    let formatted = format_messages(messages, IS_OAUTH);
    let formatted_tools = format_tools(tools, IS_OAUTH, CACHE_TTL_1H);

    let request = ApiRequest {
        model: model.to_string(),
        max_tokens,
        system: jcode_provider_anthropic::build_system_param(system, IS_OAUTH, CACHE_TTL_1H),
        messages: formatted,
        tools: if formatted_tools.is_empty() {
            None
        } else {
            Some(formatted_tools)
        },
        metadata: None,
        // Copilot exposes reasoning through `output_config.effort`; the
        // `thinking` block with an explicit token budget is not accepted here.
        thinking: None,
        output_config: reasoning_effort.map(|effort| jcode_provider_anthropic::ApiOutputConfig {
            effort: effort.to_string(),
        }),
        temperature: None,
        service_tier: None,
        stream: true,
    };

    // The request is plain owned data, so serialization cannot fail.
    serde_json::to_value(request).unwrap_or(Value::Null)
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
    fn request_uses_the_anthropic_wire_shape_not_the_openai_one() {
        let body = build_request(
            "claude-sonnet-4.6",
            "be terse",
            &[user("hi")],
            &[],
            4096,
            None,
        );
        // `system` is a top-level parameter in the Messages API, never a message
        // with role "system" as in Chat Completions.
        assert!(body.get("system").is_some());
        let messages = body["messages"].as_array().unwrap();
        assert!(messages.iter().all(|m| m["role"] != "system"));
        assert_eq!(body["max_tokens"], 4096);
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn tools_are_omitted_entirely_when_there_are_none() {
        let body = build_request("claude-sonnet-4.6", "", &[user("hi")], &[], 1024, None);
        // An empty array is not the same as absent; Anthropic rejects `tools: []`.
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn reasoning_effort_is_sent_as_output_config() {
        let body = build_request(
            "claude-sonnet-4.6",
            "",
            &[user("hi")],
            &[],
            1024,
            Some("high"),
        );
        assert_eq!(body["output_config"]["effort"], "high");
        // A `thinking` budget block alongside it would be rejected.
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn no_reasoning_effort_means_no_output_config() {
        let body = build_request("claude-sonnet-4.6", "", &[user("hi")], &[], 1024, None);
        assert!(body.get("output_config").is_none());
    }

    #[test]
    fn tools_are_serialized_when_present() {
        let tools = vec![ToolDefinition {
            name: "read_file".to_string(),
            description: "Read a file".to_string(),
            input_schema: serde_json::json!({"type": "object", "properties": {}}),
        }];
        let body = build_request("claude-sonnet-4.6", "", &[user("hi")], &tools, 1024, None);
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "read_file");
        // Anthropic names the schema field `input_schema`, not `parameters`.
        assert!(tools[0].get("input_schema").is_some());
    }
}
