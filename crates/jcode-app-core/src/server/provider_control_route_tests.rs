use super::*;
use crate::message::{Message, ToolDefinition};
use crate::provider::{EventStream, RuntimeKey};
use crate::tool::Registry;
use async_trait::async_trait;
use std::sync::{Arc, Mutex as StdMutex};

struct EmptyActiveRuntimeProvider {
    selected_model: StdMutex<String>,
}

#[async_trait]
impl Provider for EmptyActiveRuntimeProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> anyhow::Result<EventStream> {
        unreachable!("route-selection test does not perform inference")
    }

    fn name(&self) -> &str {
        "deferred-auth"
    }

    fn model(&self) -> String {
        self.selected_model.lock().unwrap().clone()
    }

    fn set_model(&self, model: &str) -> anyhow::Result<()> {
        let model = model
            .split_once(':')
            .map(|(_, model)| model)
            .unwrap_or(model)
            .trim();
        *self.selected_model.lock().unwrap() = model.to_string();
        Ok(())
    }

    fn available_models_for_switching(&self) -> Vec<String> {
        Vec::new()
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(Self {
            selected_model: StdMutex::new(self.model()),
        })
    }
}

#[tokio::test]
async fn structured_route_is_not_blocked_by_empty_active_runtime_catalog() {
    let provider: Arc<dyn Provider> = Arc::new(EmptyActiveRuntimeProvider {
        selected_model: StdMutex::new("claude-opus-5".to_string()),
    });
    let agent = Arc::new(Mutex::new(Agent::new(provider, Registry::empty())));
    let (client_event_tx, mut client_event_rx) = mpsc::unbounded_channel();
    let selection = RouteSelection {
        model: "gpt-5.6-sol".to_string(),
        runtime_key: RuntimeKey::Copilot,
        api_method: "copilot".to_string(),
        provider_label: "Copilot".to_string(),
        detail: String::new(),
    };

    handle_set_route(7, selection, &agent, &client_event_tx).await;

    assert!(matches!(
        client_event_rx.recv().await,
        Some(ServerEvent::ModelChanged {
            id: 7,
            model,
            error: None,
            ..
        }) if model == "gpt-5.6-sol"
    ));
}
