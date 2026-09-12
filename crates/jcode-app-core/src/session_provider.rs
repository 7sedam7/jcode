//! Lock-free access to each live session's provider.
//!
//! A coordinator holds its `Agent` mutex for the duration of a turn, including
//! while its `swarm` tool asks the server to spawn a worker. The server therefore
//! cannot lock the coordinator to fork its provider or inspect its refreshed
//! model catalog. This registry stores weak provider handles by session id so the
//! spawn control plane can do both without waiting on the agent lock.

use crate::provider::Provider;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, RwLock, Weak};

static SESSION_PROVIDERS: LazyLock<RwLock<HashMap<String, Weak<dyn Provider>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Register the provider currently owned by `session_id`.
pub(crate) fn record_session_provider(session_id: &str, provider: &Arc<dyn Provider>) {
    let mut providers = SESSION_PROVIDERS
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    providers.retain(|_, provider| provider.strong_count() > 0);
    providers.insert(session_id.to_string(), Arc::downgrade(provider));
}

/// Return a live session provider without taking that session's `Agent` lock.
pub(crate) fn session_provider(session_id: &str) -> Option<Arc<dyn Provider>> {
    SESSION_PROVIDERS
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(session_id)?
        .upgrade()
}

/// Move a provider registration when an existing agent resumes another session.
pub(crate) fn move_session_provider(
    old_session_id: &str,
    new_session_id: &str,
    provider: &Arc<dyn Provider>,
) {
    let mut providers = SESSION_PROVIDERS
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let provider_weak = Arc::downgrade(provider);
    if providers
        .get(old_session_id)
        .is_some_and(|registered| Weak::ptr_eq(registered, &provider_weak))
    {
        providers.remove(old_session_id);
    }
    providers.insert(new_session_id.to_string(), provider_weak);
}

/// Remove a session only if it still points at `provider`.
pub(crate) fn forget_session_provider(session_id: &str, provider: &Arc<dyn Provider>) {
    let mut providers = SESSION_PROVIDERS
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let weak = Arc::downgrade(provider);
    if providers
        .get(session_id)
        .is_some_and(|registered| Weak::ptr_eq(registered, &weak))
    {
        providers.remove(session_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Message, ToolDefinition};
    use crate::provider::EventStream;
    use anyhow::Result;
    use async_trait::async_trait;

    struct StubProvider(&'static str);

    #[async_trait]
    impl Provider for StubProvider {
        async fn complete(
            &self,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            _system: &str,
            _resume_session_id: Option<&str>,
        ) -> Result<EventStream> {
            unreachable!("provider registry test does not call complete")
        }

        fn name(&self) -> &str {
            self.0
        }

        fn fork(&self) -> Arc<dyn Provider> {
            Arc::new(Self(self.0))
        }
    }

    #[test]
    fn provider_registration_moves_and_replaces_the_previous_owner() {
        let first: Arc<dyn Provider> = Arc::new(StubProvider("first"));
        let second: Arc<dyn Provider> = Arc::new(StubProvider("second"));
        let old = "session-provider-old";
        let new = "session-provider-new";

        record_session_provider(old, &first);
        assert_eq!(
            session_provider(old).as_deref().map(Provider::name),
            Some("first")
        );

        move_session_provider(old, new, &first);
        assert!(session_provider(old).is_none());
        assert_eq!(
            session_provider(new).as_deref().map(Provider::name),
            Some("first")
        );

        record_session_provider(new, &second);
        assert_eq!(
            session_provider(new).as_deref().map(Provider::name),
            Some("second")
        );

        forget_session_provider(new, &first);
        assert_eq!(
            session_provider(new).as_deref().map(Provider::name),
            Some("second")
        );
        forget_session_provider(new, &second);
        assert!(session_provider(new).is_none());
    }
}
