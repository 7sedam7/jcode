//! SSE decoding for the Copilot `/responses` route.
//!
//! Copilot serves the OpenAI Responses event stream here, so decoding reuses
//! the OpenAI provider's stream adapter rather than reimplementing it.

use anyhow::Result;
use futures::StreamExt;
use jcode_message_types::StreamEvent;
use jcode_provider_openai::stream::OpenAIResponsesStream;
use tokio::sync::mpsc;

/// Decode a Responses-API SSE response into [`StreamEvent`]s.
pub async fn process_responses_sse_stream(
    resp: reqwest::Response,
    tx: mpsc::Sender<Result<StreamEvent>>,
) -> Result<()> {
    let sse_chunk_timeout = jcode_base::provider::stream_idle_timeout();
    let mut stream = OpenAIResponsesStream::new(resp.bytes_stream());
    let mut saw_any_event = false;

    loop {
        let next = match tokio::time::timeout(sse_chunk_timeout, stream.next()).await {
            Ok(next) => next,
            Err(_) => {
                jcode_base::logging::warn(&format!(
                    "Copilot responses SSE stream timed out (no data for {}s, saw_data={})",
                    sse_chunk_timeout.as_secs(),
                    saw_any_event
                ));
                anyhow::bail!(
                    "Stream read timeout: no data received for {} seconds",
                    sse_chunk_timeout.as_secs()
                );
            }
        };

        let Some(event) = next else { break };
        saw_any_event = true;
        if tx.send(event).await.is_err() {
            // Receiver dropped: the turn was cancelled.
            return Ok(());
        }
    }

    if !saw_any_event {
        anyhow::bail!("Copilot returned an empty stream");
    }

    Ok(())
}
