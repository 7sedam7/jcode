//! Parses a real `/models` response captured from the Copilot API and asserts
//! the routing and limits jcode derives from it.
use jcode_base::auth::copilot::{CopilotEndpoint, CopilotModelInfo};

#[derive(serde::Deserialize)]
struct Catalog {
    data: Vec<CopilotModelInfo>,
}

fn catalog() -> Vec<CopilotModelInfo> {
    let raw = include_str!("fixtures/copilot_models_live.json");
    serde_json::from_str::<Catalog>(raw)
        .expect("live catalog parses")
        .data
}

fn find(models: &[CopilotModelInfo], id: &str) -> CopilotModelInfo {
    models
        .iter()
        .find(|m| m.id == id)
        .unwrap_or_else(|| panic!("{id} missing"))
        .clone()
}

#[test]
fn responses_only_models_do_not_route_to_chat_completions() {
    // Sending these to /chat/completions returns HTTP 400
    // `unsupported_api_for_model`, which is what jcode used to do for every model.
    let models = catalog();
    for id in [
        "gpt-5.5",
        "gpt-5.6-sol",
        "gpt-5.6-luna",
        "gpt-5.6-terra",
        "gpt-5.3-codex",
        "gpt-5.4-mini",
        "grok-4.5",
        "mai-code-1-flash-picker",
    ] {
        assert_eq!(
            find(&models, id).endpoint(),
            CopilotEndpoint::Responses,
            "{id} must not be routed to /chat/completions"
        );
    }
}

#[test]
fn claude_models_route_to_the_native_messages_api() {
    let models = catalog();
    for id in ["claude-sonnet-4.6", "claude-opus-4.6", "claude-haiku-4.5"] {
        assert_eq!(
            find(&models, id).endpoint(),
            CopilotEndpoint::Messages,
            "{id}"
        );
    }
}

#[test]
fn legacy_models_without_advertised_endpoints_use_chat_completions() {
    let models = catalog();
    for id in ["gpt-4o", "gpt-4.1", "gpt-4"] {
        assert!(find(&models, id).supported_endpoints.is_empty(), "{id}");
        assert_eq!(
            find(&models, id).endpoint(),
            CopilotEndpoint::ChatCompletions,
            "{id}"
        );
    }
}

#[test]
fn live_context_window_beats_the_hardcoded_table() {
    let models = catalog();
    // jcode's table said 128k; the account really gets 1M. Understating it made
    // jcode compact roughly 8x more often than necessary.
    assert_eq!(
        find(&models, "claude-sonnet-4.6").max_context_window_tokens(),
        Some(1_000_000)
    );
    // ...and it overstated the legacy models, risking overflow.
    assert_eq!(
        find(&models, "gpt-4").max_context_window_tokens(),
        Some(32_768)
    );
    assert_eq!(
        find(&models, "gpt-3.5-turbo").max_context_window_tokens(),
        Some(16_384)
    );
}

#[test]
fn output_caps_are_read_from_the_catalog() {
    let models = catalog();
    assert_eq!(
        find(&models, "claude-sonnet-4.6").max_output_tokens(),
        Some(64_000)
    );
    assert_eq!(find(&models, "gpt-5.5").max_output_tokens(), Some(128_000));
}
