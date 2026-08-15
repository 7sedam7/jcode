//! Anthropic Messages-API SSE decoding, shared across providers.
//!
//! The wire format is a property of the Messages API, not of `api.anthropic.com`.
//! GitHub Copilot serves the same protocol at `/v1/messages` for Claude models,
//! so this decoder is public and provider-agnostic: it performs no I/O and
//! knows nothing about authentication or base URLs.

use crate::{map_tool_name_from_oauth, strip_1m_suffix};
use jcode_message_types::StreamEvent;

use crate::sse_types::{
    ApiContentBlockStart, ApiDelta, ContentBlockDeltaEvent, ContentBlockStartEvent,
    MessageDeltaEvent, MessageStartEvent,
};

/// Accumulator for tool_use blocks (input comes in chunks)
pub struct ToolUseAccumulator {
    input_json: String,
}

/// Parse a single SSE event from the buffer
pub fn parse_sse_event(buffer: &mut String) -> Option<SseEvent> {
    // Look for complete event (ends with double newline)
    let event_end = buffer.find("\n\n")?;
    let event_str = buffer[..event_end].to_string();
    buffer.drain(..event_end + 2);

    let mut event_type = String::new();
    let mut data = String::new();

    for line in event_str.lines() {
        if let Some(rest) = line.strip_prefix("event: ") {
            event_type = rest.to_string();
        } else if let Some(rest) = jcode_base::util::sse_data_line(line) {
            data = rest.to_string();
        }
    }

    if event_type.is_empty() && data.is_empty() {
        return None;
    }

    Some(SseEvent { event_type, data })
}

/// SSE event from the stream
pub struct SseEvent {
    pub event_type: String,
    pub data: String,
}

/// Mutable accumulator state threaded through [`process_sse_event`] across a
/// single SSE response stream.
#[derive(Default)]
pub struct SseStreamState {
    pub current_tool_use: Option<ToolUseAccumulator>,
    pub current_thinking_block: bool,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
    /// Lowercased base id of the model we asked for, so `message_start` can flag
    /// a silent server-side substitution (e.g. an unavailable id aliased to a
    /// different model). Empty when unknown (e.g. in unit tests).
    pub requested_model_base: String,
    /// Set once we have warned about a substitution, so we only warn per stream.
    pub warned_model_substitution: bool,
}

/// Process an SSE event and return StreamEvents if applicable
pub fn process_sse_event(
    event: &SseEvent,
    state: &mut SseStreamState,
    is_oauth: bool,
) -> Vec<StreamEvent> {
    let mut events = Vec::new();

    match event.event_type.as_str() {
        "message_start" => {
            // Extract usage from message_start (includes cache info)
            if let Ok(parsed) = serde_json::from_str::<MessageStartEvent>(&event.data) {
                // The server echoes the model that actually served the request.
                // Log it so we can confirm there was no silent server-side
                // substitution (and surface it under JCODE_LOG_SERVED_MODEL).
                if let Some(served) = parsed.message.model.as_deref() {
                    jcode_base::logging::info(&format!("Anthropic served model={}", served));
                    if std::env::var("JCODE_LOG_SERVED_MODEL").is_ok() {
                        eprintln!("[anthropic] served model={served}");
                    }
                    // Anthropic can silently alias an unavailable/retired model
                    // id to a different model (observed: claude-fable-5 ->
                    // claude-haiku-4-5). That is a correctness hazard: the user
                    // believes they are on the requested flagship. Warn loudly
                    // once per stream when the served base id differs.
                    let served_base = strip_1m_suffix(served).to_ascii_lowercase();
                    if !state.requested_model_base.is_empty()
                        && !state.warned_model_substitution
                        && served_base != state.requested_model_base
                    {
                        state.warned_model_substitution = true;
                        jcode_base::logging::warn(&format!(
                            "Anthropic served a DIFFERENT model than requested: requested '{}', served '{}'. The requested model is likely unavailable and is being substituted server-side.",
                            state.requested_model_base, served_base
                        ));
                        events.push(StreamEvent::StatusDetail {
                            detail: format!(
                                "⚠ Anthropic served '{}' instead of requested '{}' (requested model unavailable)",
                                served_base, state.requested_model_base
                            ),
                        });
                    }
                }
                if let Some(usage) = parsed.message.usage {
                    state.input_tokens = usage.input_tokens.map(|t| t as u64);
                    state.cache_read_input_tokens = usage.cache_read_input_tokens.map(|t| t as u64);
                    state.cache_creation_input_tokens =
                        usage.cache_creation_input_tokens.map(|t| t as u64);
                    if let Some(tier) = usage.service_tier.as_deref() {
                        jcode_base::logging::info(&format!(
                            "Anthropic granted service_tier={}",
                            tier
                        ));
                        if std::env::var("JCODE_LOG_SERVICE_TIER").is_ok() {
                            eprintln!("[anthropic] granted service_tier={tier}");
                        }
                    }
                }
            }
        }
        "content_block_start" => {
            if let Ok(parsed) = serde_json::from_str::<ContentBlockStartEvent>(&event.data) {
                match parsed.content_block {
                    ApiContentBlockStart::Text { .. } => {
                        // Text block starting - nothing to emit yet
                    }
                    ApiContentBlockStart::Thinking { _thinking, .. } => {
                        state.current_thinking_block = true;
                        events.push(StreamEvent::ThinkingStart);
                        if !_thinking.is_empty() {
                            events.push(StreamEvent::ThinkingDelta(_thinking));
                        }
                    }
                    ApiContentBlockStart::RedactedThinking { .. } => {
                        state.current_thinking_block = true;
                        events.push(StreamEvent::ThinkingStart);
                    }
                    ApiContentBlockStart::ToolUse { id, name } => {
                        let mapped_name = if is_oauth {
                            map_tool_name_from_oauth(&name)
                        } else {
                            name.clone()
                        };
                        // Start accumulating tool use
                        state.current_tool_use = Some(ToolUseAccumulator {
                            input_json: String::new(),
                        });
                        events.push(StreamEvent::ToolUseStart {
                            id,
                            name: mapped_name,
                        });
                    }
                    ApiContentBlockStart::Unknown => {
                        // Newer/unsupported block type. Parsing succeeded, so
                        // the rest of the stream stays intact; there is simply
                        // nothing for this build to surface.
                        jcode_base::logging::warn(
                            "Anthropic stream sent an unrecognized content_block_start type; ignoring the block",
                        );
                    }
                }
            }
        }
        "content_block_delta" => {
            if let Ok(parsed) = serde_json::from_str::<ContentBlockDeltaEvent>(&event.data) {
                match parsed.delta {
                    ApiDelta::Text { text } => {
                        events.push(StreamEvent::TextDelta(text));
                    }
                    ApiDelta::InputJson { partial_json } => {
                        if let Some(tool) = state.current_tool_use.as_mut() {
                            tool.input_json.push_str(&partial_json);
                        }
                        events.push(StreamEvent::ToolInputDelta(partial_json));
                    }
                    ApiDelta::Thinking { thinking } => {
                        events.push(StreamEvent::ThinkingDelta(thinking));
                    }
                    ApiDelta::Signature { signature } => {
                        events.push(StreamEvent::ThinkingSignatureDelta(signature));
                    }
                }
            }
        }
        "content_block_stop" => {
            // If we were accumulating a tool_use, it's complete now
            if state.current_tool_use.take().is_some() {
                events.push(StreamEvent::ToolUseEnd);
            } else if state.current_thinking_block {
                state.current_thinking_block = false;
                events.push(StreamEvent::ThinkingEnd);
            }
        }
        "message_delta" => {
            if let Ok(parsed) = serde_json::from_str::<MessageDeltaEvent>(&event.data) {
                if let Some(usage) = parsed.usage {
                    state.output_tokens = usage.output_tokens.map(|t| t as u64);
                }
                if let Some(stop_reason) = parsed.delta.stop_reason {
                    events.push(StreamEvent::MessageEnd {
                        stop_reason: Some(stop_reason),
                    });
                }
            }
        }
        "message_stop" => {
            // Final message stop - we may have already sent MessageEnd via message_delta
        }
        "ping" => {
            // Keepalive. Surface it as a phase event instead of swallowing it:
            // during silent reasoning phases (adaptive thinking with hidden or
            // summarized display) pings can be the only upstream traffic, and
            // downstream consumers (the TUI stall guard) need to see *some*
            // event to know the stream is alive (issue #451).
            events.push(StreamEvent::ConnectionPhase {
                phase: jcode_message_types::ConnectionPhase::Streaming,
            });
        }
        "error" => {
            jcode_base::logging::error(&format!("Anthropic stream error: {}", event.data));
            events.push(StreamEvent::Error {
                message: event.data.clone(),
                retry_after_secs: None,
            });
        }
        _ => {
            // Unknown event type, ignore
        }
    }

    events
}
