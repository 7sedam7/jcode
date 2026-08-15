//! Live Copilot model catalog: what each model can do, straight from the API.
//!
//! Copilot advertises per model which wire protocol it serves and what its real
//! token limits are. Those facts used to be hardcoded in jcode, which broke two
//! ways: models that do not serve `/chat/completions` were unreachable, and
//! context budgets bore no relation to the account's actual entitlement.

use std::collections::HashMap;

use jcode_base::auth::copilot::{CopilotEndpoint, CopilotModelInfo};

/// What the API says about one model.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ModelSpec {
    /// Wire protocol to send this model's requests over.
    pub endpoint: CopilotEndpoint,
    /// Total context window, when advertised.
    pub context_window: Option<usize>,
    /// Output-token cap, when advertised.
    pub max_output_tokens: Option<usize>,
    /// Reasoning-effort values this model accepts, in the catalog's order.
    pub reasoning_efforts: Vec<String>,
    /// Whether the model accepts image input.
    pub supports_vision: bool,
    /// Whether the model can call tools.
    pub supports_tool_calls: bool,
}

impl ModelSpec {
    pub fn from_info(info: &CopilotModelInfo) -> Self {
        Self {
            endpoint: info.endpoint(),
            context_window: info.max_context_window_tokens(),
            max_output_tokens: info.max_output_tokens(),
            reasoning_efforts: info.reasoning_efforts().to_vec(),
            supports_vision: info.supports_vision(),
            supports_tool_calls: info.supports_tool_calls().unwrap_or(false),
        }
    }
}

/// Model id -> capabilities, as last reported by `/models`.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct CatalogSpecs {
    specs: HashMap<String, ModelSpec>,
}

impl CatalogSpecs {
    pub fn from_models(models: &[CopilotModelInfo]) -> Self {
        Self {
            specs: models
                .iter()
                .map(|info| (info.id.clone(), ModelSpec::from_info(info)))
                .collect(),
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &ModelSpec)> {
        self.specs.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.specs.is_empty()
    }

    pub fn get(&self, model: &str) -> Option<&ModelSpec> {
        self.specs.get(model)
    }

    /// Drop what we believe about `model`, so the next lookup refetches.
    ///
    /// Used when Copilot rejects the endpoint we routed to: the spec is not
    /// missing but stale, and only a refetch can correct it.
    pub fn forget(&mut self, model: &str) -> bool {
        self.specs.remove(model).is_some()
    }

    /// Endpoint for a model, falling back to `/chat/completions` for models the
    /// catalog has not described (offline start, or a model the user pinned by
    /// hand). That fallback matches how Copilot treats models that predate
    /// `supported_endpoints`.
    pub fn endpoint_for(&self, model: &str) -> CopilotEndpoint {
        self.get(model)
            .map(|spec| spec.endpoint)
            .unwrap_or(CopilotEndpoint::ChatCompletions)
    }

    pub fn context_window_for(&self, model: &str) -> Option<usize> {
        self.get(model).and_then(|spec| spec.context_window)
    }

    pub fn max_output_tokens_for(&self, model: &str) -> Option<usize> {
        self.get(model).and_then(|spec| spec.max_output_tokens)
    }

    /// Reasoning efforts a model accepts. Empty means the model takes none, or
    /// the catalog has not described it yet.
    pub fn reasoning_efforts_for(&self, model: &str) -> &[String] {
        self.get(model)
            .map(|spec| spec.reasoning_efforts.as_slice())
            .unwrap_or_default()
    }

    pub fn supports_vision(&self, model: &str) -> bool {
        self.get(model).is_some_and(|spec| spec.supports_vision)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(id: &str, endpoints: &[&str], context: Option<usize>) -> CopilotModelInfo {
        let json = serde_json::json!({
            "id": id,
            "supported_endpoints": endpoints,
            "capabilities": { "limits": { "max_context_window_tokens": context } },
        });
        serde_json::from_value(json).expect("valid model info")
    }

    #[test]
    fn prefers_messages_over_chat_completions_regardless_of_order() {
        // Copilot lists the same Claude family in both orders, so array
        // position must not decide the route.
        let listed_first = info("a", &["/v1/messages", "/chat/completions"], None);
        let listed_last = info("b", &["/chat/completions", "/v1/messages"], None);
        assert_eq!(listed_first.endpoint(), CopilotEndpoint::Messages);
        assert_eq!(listed_last.endpoint(), CopilotEndpoint::Messages);
    }

    #[test]
    fn prefers_responses_over_chat_completions() {
        let model = info("a", &["/chat/completions", "/responses"], None);
        assert_eq!(model.endpoint(), CopilotEndpoint::Responses);
    }

    #[test]
    fn responses_only_model_routes_to_responses() {
        // These 400 with `unsupported_api_for_model` on /chat/completions.
        let model = info("gpt-5.5", &["/responses", "ws:/responses"], None);
        assert_eq!(model.endpoint(), CopilotEndpoint::Responses);
    }

    #[test]
    fn websocket_variants_do_not_influence_routing() {
        // Every real model advertising `ws:/responses` also advertises the HTTP
        // `/responses` route, and the websocket transport must not be mistaken
        // for one of the HTTP paths.
        let model = info("gpt-5.5", &["ws:/responses", "/responses"], None);
        assert_eq!(model.endpoint(), CopilotEndpoint::Responses);
    }

    #[test]
    fn websocket_only_model_falls_back_to_chat_completions() {
        // Hypothetical: no catalog entry is websocket-only today. With no HTTP
        // route advertised there is nothing better to choose.
        let model = info("a", &["ws:/responses"], None);
        assert_eq!(model.endpoint(), CopilotEndpoint::ChatCompletions);
    }

    #[test]
    fn model_without_advertised_endpoints_uses_chat_completions() {
        let model = info("gpt-4o", &[], None);
        assert_eq!(model.endpoint(), CopilotEndpoint::ChatCompletions);
    }

    #[test]
    fn unknown_model_falls_back_to_chat_completions() {
        let specs = CatalogSpecs::default();
        assert_eq!(
            specs.endpoint_for("never-seen"),
            CopilotEndpoint::ChatCompletions
        );
        assert_eq!(specs.context_window_for("never-seen"), None);
    }

    #[test]
    fn serves_live_context_window() {
        let specs = CatalogSpecs::from_models(&[info(
            "claude-sonnet-4.6",
            &["/v1/messages"],
            Some(1_000_000),
        )]);
        assert_eq!(
            specs.context_window_for("claude-sonnet-4.6"),
            Some(1_000_000)
        );
        assert_eq!(
            specs.endpoint_for("claude-sonnet-4.6"),
            CopilotEndpoint::Messages
        );
    }
}
