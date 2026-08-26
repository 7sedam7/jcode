use jcode_message_types::{
    ContentBlock, Message as ChatMessage, Role, TOOL_OUTPUT_MISSING_TEXT, ToolDefinition,
    sanitize_tool_id,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

/// `X-GitHub-Api-Version` sent on Copilot API requests.
///
/// Not cosmetic: measured against the live API, `2025-04-01` omits the
/// `billing.token_prices` block from `/models` for every model, so jcode cannot
/// know what a request costs. `2026-06-01` returns it for all of them, and is
/// the version OpenCode sends.
pub const COPILOT_API_VERSION: &str = "2026-06-01";

/// Default model id. This must be a **Copilot catalog** id (dot-separated,
/// e.g. `claude-sonnet-4.6`), not the Anthropic-native hyphenated form: the
/// Copilot API rejects the latter with HTTP 400 `model_not_supported`
/// (issue #640). Keep this in sync with the head of [`FALLBACK_MODELS`].
pub const DEFAULT_MODEL: &str = "claude-sonnet-4.6";

/// Legacy static model list.
///
/// **Deprecated** — the set of reachable Copilot models depends on the OAuth
/// app the token was minted under and is only knowable from the live
/// `GET /models` response.  This list is kept only for backward-compatible
/// test stubs; production code must not gate behaviour on membership here.
pub const FALLBACK_MODELS: &[&str] = &[
    "claude-sonnet-4.6",
    "claude-sonnet-4.5",
    "claude-haiku-4.5",
    "claude-opus-4.6",
    "claude-opus-4.5",
    "claude-sonnet-4",
    "gpt-5.4",
    "gpt-5.3-codex",
    "gpt-5.1-codex",
    "gpt-5.1",
    "gpt-5-mini",
    "gpt-4.1",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedCatalog {
    pub models: Vec<String>,
    pub fetched_at_rfc3339: String,
}

/// **Deprecated** — see [`FALLBACK_MODELS`].  Returns `true` when the id
/// appears in the legacy static list; callers should derive from the live
/// catalog instead.
pub fn is_known_display_model(model: &str) -> bool {
    FALLBACK_MODELS.contains(&model)
}

pub fn max_token_parameter_for_model(model: &str) -> &'static str {
    let normalized = model.trim().to_ascii_lowercase();
    if normalized.starts_with("gpt-5") {
        "max_completion_tokens"
    } else {
        "max_tokens"
    }
}

pub fn add_max_token_parameter(body: &mut Value, model: &str, max_tokens: u32) {
    body[max_token_parameter_for_model(model)] = json!(max_tokens);
}

/// Build OpenAI-compatible messages array from jcode's message format.
///
/// Properly pairs tool_use blocks (in assistant messages) with their
/// corresponding tool_result blocks (in user messages), handling out-of-order
/// results and missing outputs.
pub fn build_messages(system: &str, messages: &[ChatMessage]) -> Vec<Value> {
    let mut result = Vec::new();
    let missing_output = format!("[Error] {}", TOOL_OUTPUT_MISSING_TEXT);
    let user_content = |mut parts: Vec<Value>| -> Option<Value> {
        parts.retain(|part| {
            part.get("type").and_then(Value::as_str) != Some("text")
                || part
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| !text.is_empty())
        });
        if parts.is_empty() {
            return None;
        }
        if parts
            .iter()
            .all(|part| part.get("type").and_then(Value::as_str) == Some("text"))
        {
            return Some(json!(
                parts
                    .iter()
                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
        Some(Value::Array(parts))
    };

    if !system.is_empty() {
        result.push(json!({
            "role": "system",
            "content": system,
        }));
    }

    let mut tool_result_last_pos: HashMap<String, usize> = HashMap::new();
    for (idx, msg) in messages.iter().enumerate() {
        if let Role::User = msg.role {
            for block in &msg.content {
                if let ContentBlock::ToolResult { tool_use_id, .. } = block {
                    tool_result_last_pos.insert(tool_use_id.clone(), idx);
                }
            }
        }
    }

    let mut tool_calls_seen: HashSet<String> = HashSet::new();
    let mut pending_tool_results: HashMap<String, String> = HashMap::new();
    let mut used_tool_results: HashSet<String> = HashSet::new();

    for (idx, msg) in messages.iter().enumerate() {
        match msg.role {
            Role::User => {
                let mut user_parts = Vec::new();
                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text, .. } => {
                            user_parts.push(json!({
                                "type": "text",
                                "text": text,
                            }));
                        }
                        ContentBlock::Image { media_type, data } => {
                            user_parts.push(json!({
                                "type": "image_url",
                                "image_url": {
                                    "url": format!("data:{};base64,{}", media_type, data),
                                },
                            }));
                        }
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } => {
                            if used_tool_results.contains(tool_use_id) {
                                continue;
                            }
                            let output = if is_error == &Some(true) {
                                format!("[Error] {}", content)
                            } else if content.is_empty() {
                                TOOL_OUTPUT_MISSING_TEXT.to_string()
                            } else {
                                content.clone()
                            };
                            if tool_calls_seen.contains(tool_use_id) {
                                result.push(json!({
                                    "role": "tool",
                                    "tool_call_id": sanitize_tool_id(tool_use_id),
                                    "content": output,
                                }));
                                used_tool_results.insert(tool_use_id.clone());
                            } else if !pending_tool_results.contains_key(tool_use_id) {
                                pending_tool_results.insert(tool_use_id.clone(), output);
                            }
                        }
                        _ => {}
                    }
                }

                if let Some(content) = user_content(user_parts) {
                    result.push(json!({
                        "role": "user",
                        "content": content,
                    }));
                }
            }
            Role::Assistant => {
                let mut content_text = String::new();
                let mut tool_calls = Vec::new();
                let mut post_tool_outputs: Vec<(String, String)> = Vec::new();
                let mut missing_tool_outputs: Vec<String> = Vec::new();

                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text, .. } => {
                            content_text.push_str(text);
                        }
                        ContentBlock::ToolUse {
                            id, name, input, ..
                        } => {
                            let args = if input.is_object() {
                                input.to_string()
                            } else {
                                "{}".to_string()
                            };
                            tool_calls.push(json!({
                                "id": sanitize_tool_id(id),
                                "type": "function",
                                "function": {
                                    "name": name,
                                    "arguments": args,
                                }
                            }));
                            tool_calls_seen.insert(id.clone());
                            if let Some(output) = pending_tool_results.remove(id) {
                                post_tool_outputs.push((id.clone(), output));
                                used_tool_results.insert(id.clone());
                            } else {
                                let has_future_output = tool_result_last_pos
                                    .get(id)
                                    .map(|pos| *pos > idx)
                                    .unwrap_or(false);
                                if !has_future_output {
                                    missing_tool_outputs.push(id.clone());
                                    used_tool_results.insert(id.clone());
                                }
                            }
                        }
                        _ => {}
                    }
                }

                let mut assistant_msg = json!({
                    "role": "assistant",
                });

                if !content_text.is_empty() {
                    assistant_msg["content"] = json!(content_text);
                }
                if !tool_calls.is_empty() {
                    assistant_msg["tool_calls"] = json!(tool_calls);
                }

                if !content_text.is_empty() || !tool_calls.is_empty() {
                    result.push(assistant_msg);

                    for (tool_call_id, output) in post_tool_outputs {
                        result.push(json!({
                            "role": "tool",
                            "tool_call_id": sanitize_tool_id(&tool_call_id),
                            "content": output,
                        }));
                    }

                    for missing_id in missing_tool_outputs {
                        result.push(json!({
                            "role": "tool",
                            "tool_call_id": sanitize_tool_id(&missing_id),
                            "content": missing_output.clone(),
                        }));
                    }
                }
            }
        }
    }

    result
}

/// Build OpenAI-compatible tools array.
pub fn build_tools(tools: &[ToolDefinition]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            // Copilot's chat-completions endpoint routes to heterogeneous
            // upstreams and, like OpenRouter, rejects combiners at the tool
            // schema root. Normalize to that conservative dialect before the
            // schema is placed in the request payload (issue #855).
            let parameters = jcode_schema_dialect::normalize(
                &t.input_schema,
                &jcode_schema_dialect::registry::OPENROUTER,
            );
            json!({
                "type": "function",
                "function": {
                    "name": &t.name,
                    // Prompt-visible. Approximate token cost for this field:
                    // t.description_token_estimate().
                    "description": &t.description,
                    "parameters": parameters,
                }
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copilot_chat_messages_preserve_image_parts() {
        let messages = vec![ChatMessage {
            role: Role::User,
            content: vec![
                ContentBlock::Text {
                    text: "describe this".to_string(),
                    cache_control: None,
                },
                ContentBlock::Image {
                    media_type: "image/png".to_string(),
                    data: "aW1hZ2U=".to_string(),
                },
            ],
            timestamp: None,
            tool_duration_ms: None,
        }];

        let built = build_messages("", &messages);
        assert_eq!(built[0]["role"], "user");
        assert_eq!(built[0]["content"][0]["type"], "text");
        assert_eq!(built[0]["content"][0]["text"], "describe this");
        assert_eq!(built[0]["content"][1]["type"], "image_url");
        assert_eq!(
            built[0]["content"][1]["image_url"]["url"],
            "data:image/png;base64,aW1hZ2U="
        );
    }

    #[test]
    fn copilot_chat_messages_skip_empty_user_text() {
        let messages = vec![ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: String::new(),
                cache_control: None,
            }],
            timestamp: None,
            tool_duration_ms: None,
        }];

        assert!(build_messages("", &messages).is_empty());
    }

    #[test]
    fn copilot_chat_messages_keep_tool_results_before_mixed_user_content() {
        let messages = vec![
            ChatMessage {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "call-1".to_string(),
                    name: "screenshot".to_string(),
                    input: json!({}),
                    thought_signature: None,
                }],
                timestamp: None,
                tool_duration_ms: None,
            },
            ChatMessage {
                role: Role::User,
                content: vec![
                    ContentBlock::Text {
                        text: "look at this".to_string(),
                        cache_control: None,
                    },
                    ContentBlock::Image {
                        media_type: "image/png".to_string(),
                        data: "aW1hZ2U=".to_string(),
                    },
                    ContentBlock::ToolResult {
                        tool_use_id: "call-1".to_string(),
                        content: "captured".to_string(),
                        is_error: None,
                    },
                ],
                timestamp: None,
                tool_duration_ms: None,
            },
        ];

        let built = build_messages("", &messages);
        assert_eq!(built[0]["role"], "assistant");
        assert_eq!(built[1]["role"], "tool");
        assert_eq!(built[2]["role"], "user");
        assert_eq!(built[2]["content"][1]["type"], "image_url");
    }

    fn swarm_shaped_tool() -> ToolDefinition {
        ToolDefinition {
            name: "swarm".to_string(),
            description: "Coordinate agents".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["action"],
                "properties": {
                    "action": {"type": "string"},
                    "message": {"type": "string"}
                },
                "anyOf": [
                    {
                        "type": "object",
                        "required": ["action", "label"],
                        "properties": {
                            "action": {"type": "string", "enum": ["spawn"]},
                            "label": {"type": "string", "minLength": 1}
                        }
                    },
                    {
                        "type": "object",
                        "required": ["action"],
                        "properties": {
                            "action": {"type": "string", "enum": ["list"]}
                        }
                    }
                ]
            }),
        }
    }

    #[test]
    fn copilot_tool_serialization_flattens_swarm_top_level_combiners() {
        let json = serde_json::to_string(&build_tools(&[swarm_shaped_tool()])).unwrap();
        let serialized: Value = serde_json::from_str(&json).unwrap();
        let parameters = &serialized[0]["function"]["parameters"];

        for combiner in ["anyOf", "oneOf", "allOf"] {
            assert!(
                parameters.get(combiner).is_none(),
                "top-level {combiner} must not reach Copilot: {parameters}"
            );
        }
    }

    #[test]
    fn copilot_tool_payload_preserves_swarm_properties_and_required_fields() {
        let tools = build_tools(&[swarm_shaped_tool()]);
        let function = &tools[0]["function"];
        let parameters = &function["parameters"];

        assert_eq!(function["name"], "swarm");
        assert_eq!(function["description"], "Coordinate agents");
        assert!(parameters["properties"]["action"].is_object());
        assert!(parameters["properties"]["message"].is_object());
        assert!(parameters["properties"]["label"].is_object());
        assert_eq!(parameters["required"], json!(["action"]));
    }
}
