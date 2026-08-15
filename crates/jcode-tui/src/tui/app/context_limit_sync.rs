//! Keeping the cached context limit in step with the provider.

use super::App;

impl App {
    /// Adopt the provider's context window if it has changed underneath us.
    ///
    /// `context_limit` is a cached copy, taken once at startup. Providers that
    /// learn their real limits from an async catalog fetch (Copilot) have not
    /// answered yet at that point, so the cached value is a fallback guess —
    /// 128k, regardless of whether the model actually allows 1M. Nothing else
    /// recomputes it until the user switches model, so without this the whole
    /// session budgets against the wrong number.
    ///
    /// Returns whether the limit moved, so the caller can redraw.
    pub(super) fn sync_context_limit_from_provider(&mut self) -> bool {
        if self.is_remote {
            return false;
        }
        let live = self.provider.context_window() as u64;
        if live == self.context_limit {
            return false;
        }
        self.context_limit = live;
        self.context_warning_shown = false;
        let compaction = self.registry.compaction();
        if let Ok(mut manager) = compaction.try_write() {
            manager.set_budget(live as usize);
        }
        true
    }
}
