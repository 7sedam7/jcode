use crate::provider::Provider;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Return the coordinator's live provider without taking its `Agent` lock.
/// Swarm tool calls run while that lock is held, so the session registry is the
/// only deadlock-free way to preserve a refreshed provider catalog for workers.
pub(super) fn for_session(
    session_id: &str,
    provider_template: &Arc<dyn Provider>,
) -> Arc<dyn Provider> {
    crate::session_provider::session_provider(session_id)
        .unwrap_or_else(|| Arc::clone(provider_template))
}

/// Resolve the live provider identity for a bare model in its active catalog.
pub(super) fn route_for_model(
    provider: &dyn Provider,
    model: &str,
    provider_key: Option<&str>,
    route_api_method: Option<&str>,
) -> Option<(String, Option<String>)> {
    provider
        .available_models_for_switching()
        .iter()
        .any(|listed| listed.eq_ignore_ascii_case(model.trim()))
        .then(|| {
            let key = provider_key
                .map(str::to_string)
                .or_else(|| crate::session::derive_session_provider_key(provider.name()))?;
            let route = route_api_method
                .map(str::to_string)
                .or_else(|| (key == "copilot").then(|| "copilot".to_string()));
            Some((key, route))
        })?
}

pub(super) fn forget_session(session_id: &str, provider: &Arc<dyn Provider>) {
    crate::session_provider::forget_session_provider(session_id, provider);
}

pub(super) fn forget_agent_session(session_id: &str, agent: &Arc<Mutex<crate::agent::Agent>>) {
    if let Ok(agent) = agent.try_lock() {
        forget_session(session_id, &agent.provider_handle());
    }
}
