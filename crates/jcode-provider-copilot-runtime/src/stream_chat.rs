//! SSE decoding for the Copilot `/chat/completions` route.
//!
//! The OpenAI-shaped chat stream, used by the legacy models that predate
//! `supported_endpoints` (gpt-4o, gpt-4.1, o3-mini and friends).

use anyhow::Result;
use jcode_message_types::StreamEvent;
use serde_json::Value;
use tokio::sync::mpsc;

pub async fn process_chat_sse_stream(
    resp: reqwest::Response,
    tx: mpsc::Sender<Result<StreamEvent>>,
) -> Result<()> {
    use futures::StreamExt;

    // Idle timeout between streamed chunks. Configurable via
    // `[provider] stream_idle_timeout_secs` / `JCODE_STREAM_IDLE_TIMEOUT_SECS`
    // so slow reasoning models don't trip a premature timeout (issue #434).
    let sse_chunk_timeout = jcode_base::provider::stream_idle_timeout();

    let mut stream = resp.bytes_stream();
    let mut buffer = String::new();
    let mut current_tool_id = String::new();
    let mut current_tool_name = String::new();
    let mut current_tool_args = String::new();
    let mut input_tokens: u64 = 0;
    let mut output_tokens: u64 = 0;
    let mut saw_any_data = false;

    loop {
        let chunk = match tokio::time::timeout(sse_chunk_timeout, stream.next()).await {
            Ok(Some(Ok(c))) => c,
            Ok(Some(Err(e))) => {
                anyhow::bail!("Stream error: {}", e);
            }
            Ok(None) => break, // stream ended normally
            Err(_) => {
                jcode_base::logging::warn(&format!(
                    "Copilot SSE stream timed out (no data for {}s, saw_data={})",
                    sse_chunk_timeout.as_secs(),
                    saw_any_data
                ));
                anyhow::bail!(
                    "Stream read timeout: no data received for {} seconds",
                    sse_chunk_timeout.as_secs()
                );
            }
        };
        saw_any_data = true;

        buffer.push_str(&String::from_utf8_lossy(&chunk));

        // Process complete SSE lines
        while let Some(line_end) = buffer.find('\n') {
            let line = buffer[..line_end].trim_end_matches('\r').to_string();
            buffer = buffer[line_end + 1..].to_string();

            if line.is_empty() || line.starts_with(':') {
                continue;
            }

            if let Some(data) = jcode_base::util::sse_data_line(&line) {
                if data.trim() == "[DONE]" {
                    // Send usage info before done
                    if input_tokens > 0 || output_tokens > 0 {
                        let _ = tx
                            .send(Ok(StreamEvent::TokenUsage {
                                input_tokens: Some(input_tokens),
                                output_tokens: Some(output_tokens),
                                cache_creation_input_tokens: None,
                                cache_read_input_tokens: None,
                            }))
                            .await;
                    }
                    jcode_base::copilot_usage::record_request(input_tokens, output_tokens, true);
                    let _ = tx
                        .send(Ok(StreamEvent::MessageEnd { stop_reason: None }))
                        .await;
                    return Ok(());
                }

                let parsed: Value = match serde_json::from_str(data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                // Extract usage if present
                if let Some(usage) = parsed.get("usage") {
                    input_tokens = usage
                        .get("prompt_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    output_tokens = usage
                        .get("completion_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                }

                // Process choices
                if let Some(choices) = parsed.get("choices").and_then(|c| c.as_array()) {
                    for choice in choices {
                        let delta = match choice.get("delta") {
                            Some(d) => d,
                            None => continue,
                        };

                        // Text content
                        if let Some(content) = delta.get("content").and_then(|c| c.as_str())
                            && !content.is_empty()
                        {
                            let _ = tx
                                .send(Ok(StreamEvent::TextDelta(content.to_string())))
                                .await;
                        }

                        // Tool calls
                        if let Some(tool_calls) = delta.get("tool_calls").and_then(|t| t.as_array())
                        {
                            for tc in tool_calls {
                                // New tool call start
                                if let Some(id) = tc.get("id").and_then(|i| i.as_str()) {
                                    // Flush previous tool call if any
                                    if !current_tool_id.is_empty() {
                                        let _ = tx.send(Ok(StreamEvent::ToolUseEnd)).await;
                                    }
                                    current_tool_id = id.to_string();
                                    current_tool_name = tc
                                        .get("function")
                                        .and_then(|f| f.get("name"))
                                        .and_then(|n| n.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    current_tool_args.clear();

                                    let _ = tx
                                        .send(Ok(StreamEvent::ToolUseStart {
                                            id: current_tool_id.clone(),
                                            name: current_tool_name.clone(),
                                        }))
                                        .await;
                                }

                                // Accumulate arguments
                                if let Some(args) = tc
                                    .get("function")
                                    .and_then(|f| f.get("arguments"))
                                    .and_then(|a| a.as_str())
                                {
                                    current_tool_args.push_str(args);
                                    let _ = tx
                                        .send(Ok(StreamEvent::ToolInputDelta(args.to_string())))
                                        .await;
                                }
                            }
                        }

                        // Finish reason
                        if let Some(finish) = choice.get("finish_reason").and_then(|f| f.as_str()) {
                            // Flush last tool call
                            if !current_tool_id.is_empty() {
                                let _ = tx.send(Ok(StreamEvent::ToolUseEnd)).await;
                                current_tool_id.clear();
                                current_tool_name.clear();
                                current_tool_args.clear();
                            }

                            let stop_reason = match finish {
                                "stop" => "end_turn",
                                "tool_calls" => "tool_use",
                                "length" => "max_tokens",
                                other => other,
                            };
                            let _ = tx
                                .send(Ok(StreamEvent::MessageEnd {
                                    stop_reason: Some(stop_reason.to_string()),
                                }))
                                .await;
                        }
                    }
                }
            }
        }
    }

    // Stream ended without [DONE]
    let _ = tx
        .send(Ok(StreamEvent::MessageEnd { stop_reason: None }))
        .await;
    Ok(())
}
