//! SSE decoding for the Copilot `/v1/messages` route.
//!
//! Copilot serves Anthropic's native Messages event stream here, so the decoder
//! is the Anthropic provider's, reused verbatim rather than reimplemented.

use anyhow::Result;
use jcode_message_types::StreamEvent;
use jcode_provider_anthropic_runtime::sse::{SseStreamState, parse_sse_event, process_sse_event};
use tokio::sync::mpsc;

/// Copilot's Messages route is not an OAuth Claude Code session.
const IS_OAUTH: bool = false;

/// Decode a Messages-API SSE response into [`StreamEvent`]s.
pub async fn process_messages_sse_stream(
    resp: reqwest::Response,
    model: &str,
    tx: mpsc::Sender<Result<StreamEvent>>,
) -> Result<()> {
    use futures::StreamExt;

    let sse_chunk_timeout = jcode_base::provider::stream_idle_timeout();
    let mut stream = resp.bytes_stream();
    let mut buffer = String::new();
    let mut saw_any_data = false;

    let mut state = SseStreamState {
        requested_model_base: model.to_ascii_lowercase(),
        ..SseStreamState::default()
    };

    loop {
        let chunk = match tokio::time::timeout(sse_chunk_timeout, stream.next()).await {
            Ok(Some(Ok(c))) => c,
            Ok(Some(Err(e))) => anyhow::bail!("Stream error: {}", e),
            Ok(None) => break,
            Err(_) => {
                jcode_base::logging::warn(&format!(
                    "Copilot messages SSE stream timed out (no data for {}s, saw_data={})",
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

        while let Some(event) = parse_sse_event(&mut buffer) {
            for stream_event in process_sse_event(&event, &mut state, IS_OAUTH) {
                if tx.send(Ok(stream_event)).await.is_err() {
                    // Receiver dropped: the turn was cancelled.
                    return Ok(());
                }
            }
        }
    }

    if !saw_any_data {
        anyhow::bail!("Copilot returned an empty stream");
    }

    Ok(())
}
