use crate::agent::Agent;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Mutex;

/// Refresh busy sessions once their current turn releases the agent lock, while
/// exposing a counter the initiating login can include in its completion barrier.
pub(super) fn spawn_deferred_auth_refreshes(agents: Vec<Arc<Mutex<Agent>>>) -> Arc<AtomicUsize> {
    let pending = Arc::new(AtomicUsize::new(agents.len()));
    for agent in agents {
        let pending = Arc::clone(&pending);
        tokio::spawn(async move {
            struct PendingGuard(Arc<AtomicUsize>);
            impl Drop for PendingGuard {
                fn drop(&mut self) {
                    self.0.fetch_sub(1, Ordering::AcqRel);
                }
            }
            let _pending_guard = PendingGuard(pending);
            let provider = {
                let agent_guard = agent.lock().await;
                agent_guard.provider_handle()
            };
            provider.on_auth_changed_preserve_current_provider();
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
            while provider.auth_model_refresh_pending() && tokio::time::Instant::now() < deadline {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            crate::bus::Bus::global().publish_models_updated();
        });
    }
    pending
}
