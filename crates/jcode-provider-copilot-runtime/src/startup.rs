//! Copilot provider startup: catalog persistence and the seat/catalog probe
//! that runs before the first request.
//!
//! Split out of `lib.rs` to keep it under the code-size budget.

use super::*;

/// The on-disk model cache.
///
/// Carries the per-model **specs**, not just the ids. Caching ids alone meant a
/// relaunch restored the picker but left every context window unknown, so the
/// provider fell back to a hardcoded 128k until the network answered — three
/// orders of magnitude below what a 1M-token model actually allows.
///
/// `specs` is `#[serde(default)]` so an ids-only cache written by an older build
/// still loads; it simply carries no specs until the next fetch rewrites it.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct PersistedCopilotCatalog {
    pub models: Vec<String>,
    #[serde(default)]
    pub specs: CatalogSpecs,
    pub fetched_at_rfc3339: String,
}

impl CopilotApiProvider {
    fn persisted_catalog_path() -> Result<std::path::PathBuf> {
        Ok(jcode_base::storage::app_config_dir()?.join("copilot_models_cache.json"))
    }

    fn load_persisted_catalog() -> Option<PersistedCopilotCatalog> {
        let path = Self::persisted_catalog_path().ok()?;
        jcode_base::storage::read_json(&path)
            .ok()
            .filter(|catalog: &PersistedCopilotCatalog| !catalog.models.is_empty())
    }

    fn persist_catalog(models: &[String], specs: &CatalogSpecs) {
        if models.is_empty() {
            return;
        }
        let Ok(path) = Self::persisted_catalog_path() else {
            return;
        };
        let payload = PersistedCopilotCatalog {
            models: models.to_vec(),
            specs: specs.clone(),
            fetched_at_rfc3339: Utc::now().to_rfc3339(),
        };
        if let Err(error) = jcode_base::storage::write_json(&path, &payload) {
            jcode_base::logging::warn(&format!(
                "Failed to persist Copilot model catalog {}: {}",
                path.display(),
                error
            ));
        }
    }

    /// Publish the catalog's context windows process-wide.
    ///
    /// `context_window()` on this provider is not the only consumer: the TUI,
    /// the compaction budget and the remote client all resolve limits through
    /// the free function in `jcode_provider_core`, which otherwise answers from
    /// a static table that is wrong for most accounts (it caps every `gpt-5*`
    /// at 128k when they serve 400k-1.05M).
    fn publish_context_limits(models: &[copilot_auth::CopilotModelInfo]) {
        let limits: std::collections::HashMap<String, usize> = models
            .iter()
            .filter_map(|m| Some((m.id.clone(), m.max_context_window_tokens()?)))
            .collect();
        jcode_provider_core::record_copilot_catalog_context_limits(limits);
    }

    /// Republish limits restored from the on-disk cache.
    fn publish_cached_context_limits(specs: &CatalogSpecs) {
        let limits: std::collections::HashMap<String, usize> = specs
            .iter()
            .filter_map(|(id, spec)| Some((id.clone(), spec.context_window?)))
            .collect();
        jcode_provider_core::record_copilot_catalog_context_limits(limits);
    }

    pub(crate) fn seed_cached_catalog(&self) {
        if let Some(catalog) = Self::load_persisted_catalog() {
            if let Ok(mut models) = self.fetched_models.write() {
                *models = catalog.models;
            }
            if let Ok(mut source) = self.catalog_source.write() {
                *source = CatalogSource::Cached;
            }
            // Restore the limits alongside the ids, so the very first turn after
            // a relaunch budgets against the model's real context window.
            if !catalog.specs.is_empty() {
                Self::publish_cached_context_limits(&catalog.specs);
                if let Ok(mut specs) = self.model_specs.write() {
                    *specs = catalog.specs;
                }
            }
        }
    }

    /// The wire protocol for `model`, refreshing the catalog if it is unknown.
    ///
    /// The catalog is authoritative about which endpoint a model serves, and
    /// `CatalogSpecs` falls back to `/chat/completions` for anything it has not
    /// described. That fallback is correct for models predating
    /// `supported_endpoints`, but wrong — and unrecoverable, a hard HTTP 400
    /// `unsupported_api_for_model` — for a model that simply is not in the
    /// catalog yet.
    ///
    /// That gap is routine rather than exotic: the daemon runs with tier
    /// detection disabled and seeds its specs from a cached catalog, and the
    /// attach-time prefetch is skipped whenever that cache already produced a
    /// model list. A model GitHub added after the cache was written is then
    /// offered by the picker while no spec describes it. Fetching once, only on
    /// that miss, closes the gap without adding a request to the common path.
    pub(crate) async fn endpoint_for_model(
        &self,
        model: &str,
        bearer: &str,
    ) -> copilot_auth::CopilotEndpoint {
        if let Ok(specs) = self.model_specs.read()
            && let Some(spec) = specs.get(model)
        {
            return spec.endpoint;
        }

        jcode_base::logging::info(&format!(
            "Copilot catalog does not describe '{model}'; refreshing it before \
             choosing an endpoint (guessing would fail with HTTP 400)"
        ));
        match copilot_auth::fetch_available_models(&self.client, bearer).await {
            Ok(models) => {
                let models: Vec<_> = models
                    .into_iter()
                    .filter(copilot_auth::CopilotModelInfo::is_usable_for_chat)
                    .collect();
                Self::publish_context_limits(&models);
                let fresh_specs = CatalogSpecs::from_models(&models);
                let endpoint = fresh_specs.endpoint_for(model);
                if let Ok(mut specs) = self.model_specs.write() {
                    *specs = fresh_specs.clone();
                }
                Self::persist_catalog(&self.available_model_ids(), &fresh_specs);
                jcode_base::logging::info(&format!(
                    "Copilot catalog refreshed: '{model}' serves {}",
                    endpoint.path()
                ));
                endpoint
            }
            Err(error) => {
                // Falling back is still better than failing outright: most
                // models do serve `/chat/completions`, and a wrong guess
                // reports Copilot's own error rather than a jcode one.
                jcode_base::logging::warn(&format!(
                    "Could not refresh the Copilot catalog to route '{model}' ({error}); \
                     falling back to {}",
                    copilot_auth::CopilotEndpoint::ChatCompletions.path()
                ));
                copilot_auth::CopilotEndpoint::ChatCompletions
            }
        }
    }

    /// Install the catalog default, unless the caller already chose a model.
    ///
    /// Tier detection is spawned at construction and finishes long after
    /// `--model`, the `/model` picker, or session restore has called
    /// `set_model`. Writing the default unconditionally therefore discarded
    /// every explicit selection and sent the whole session to one model.
    pub(crate) fn apply_catalog_default(&self, catalog_default: String, all_ids: &[String]) {
        if !self
            .model_explicitly_selected
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            self.write_model(catalog_default);
            return;
        }

        let selected = self.model();
        if !all_ids.is_empty() && !all_ids.iter().any(|id| id == &selected) {
            jcode_base::logging::warn(&format!(
                "Selected Copilot model '{}' is not in this account's catalog; \
                 requests will likely fail. Available: [{}]",
                selected,
                all_ids.join(", ")
            ));
        } else {
            jcode_base::logging::info(&format!(
                "Keeping explicitly selected Copilot model '{selected}' \
                 (catalog default would have been '{catalog_default}')"
            ));
        }
    }

    /// Fetch the account's model catalog and pick a default it can reach.
    ///
    /// Always fetches, even when the model is pinned: the catalog also feeds the
    /// picker and the context-window limits, so skipping the fetch would leave
    /// jcode guessing at both.
    pub async fn detect_tier_and_set_default(&self) {
        let detect_start = std::time::Instant::now();
        let pinned_model = std::env::var("JCODE_COPILOT_MODEL")
            .ok()
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty());

        let bearer_start = std::time::Instant::now();
        let bearer = match self.get_bearer_token().await {
            Ok(t) => t,
            Err(e) => {
                jcode_base::logging::info(&format!(
                    "Copilot tier detection: failed to get bearer token after {}ms: {}",
                    bearer_start.elapsed().as_millis(),
                    e
                ));
                self.mark_init_done();
                return;
            }
        };

        // Ask GitHub what this seat is and where its traffic goes, before the
        // catalog fetch: an enterprise seat is served from a different host, so
        // discovering it afterwards would send the first requests to the wrong
        // endpoint. A failure here is not fatal — the deployment default still
        // works for ordinary github.com seats.
        match copilot_auth_enterprise::fetch_user_info(&self.client, &bearer).await {
            Ok(info) => {
                jcode_base::logging::info(&format!(
                    "Copilot seat: plan={} sku={} orgs=[{}] api={}",
                    info.copilot_plan,
                    info.access_type_sku,
                    info.organization_login_list.join(", "),
                    info.api_base().unwrap_or("(not reported)")
                ));
                if let Ok(mut account) = self.account_type.write() {
                    *account = info.account_type();
                }
            }
            Err(e) => jcode_base::logging::info(&format!(
                "Copilot seat lookup failed ({e}); using the {} endpoint",
                copilot_auth_enterprise::api_base()
            )),
        }

        let fetch_start = std::time::Instant::now();
        match copilot_auth::fetch_available_models(&self.client, &bearer).await {
            Ok(models) => {
                // The catalog also lists embedding and other non-chat models.
                // They advertise no token limits and no tool support, so an
                // agent turn sent to one fails in a way that reads like a jcode
                // bug. OpenCode applies the same filter before exposing models.
                let models: Vec<_> = models
                    .into_iter()
                    .filter(copilot_auth::CopilotModelInfo::is_usable_for_chat)
                    .collect();
                copilot_auth::record_catalog_billing(&models);
                Self::publish_context_limits(&models);
                let picker_models: Vec<String> = models
                    .iter()
                    .filter(|m| m.model_picker_enabled)
                    .map(|m| m.id.clone())
                    .collect();
                let all_ids: Vec<String> = models.iter().map(|m| m.id.clone()).collect();
                let default = copilot_auth::choose_default_model(&models);
                jcode_base::logging::info(&format!(
                    "Copilot tier detection: bearer={}ms, fetch_models={}ms, total={}ms, {} total, {} picker-enabled, default -> {}. Picker: [{}]. All: [{}]",
                    bearer_start.elapsed().as_millis(),
                    fetch_start.elapsed().as_millis(),
                    detect_start.elapsed().as_millis(),
                    all_ids.len(),
                    picker_models.len(),
                    default,
                    picker_models.join(", "),
                    all_ids.join(", ")
                ));
                // A pinned model wins, but only if the account can reach it.
                // Honoring an unreachable pin just defers the failure to the
                // first request, where it surfaces as an opaque HTTP 400.
                let chosen = match pinned_model {
                    Some(pinned) if all_ids.contains(&pinned) => pinned,
                    Some(pinned) => {
                        jcode_base::logging::warn(&format!(
                            "JCODE_COPILOT_MODEL='{}' is not in this account's Copilot catalog; \
                             using '{}' instead. Available: [{}]",
                            pinned,
                            default,
                            all_ids.join(", ")
                        ));
                        default
                    }
                    None => default,
                };
                // Detection runs asynchronously, so a `--model` flag, a `/model`
                // pick, or a session restore has usually already landed by now.
                // Overwriting it here is what silently routed every turn to the
                // catalog default regardless of what the user selected.
                self.apply_catalog_default(chosen, &all_ids);
                let display_models = if picker_models.is_empty() {
                    all_ids
                } else {
                    picker_models
                };
                // Blocking writes, not `try_write`: a failed try silently drops
                // the catalog and leaves the picker empty for the whole session.
                if let Ok(mut fm) = self.fetched_models.write() {
                    *fm = display_models;
                }
                if let Ok(mut source) = self.catalog_source.write() {
                    *source = CatalogSource::Live;
                }
                let fresh_specs = CatalogSpecs::from_models(&models);
                if let Ok(mut specs) = self.model_specs.write() {
                    *specs = fresh_specs.clone();
                }
                Self::persist_catalog(&self.available_model_ids(), &fresh_specs);
            }
            Err(e) => {
                jcode_base::logging::info(&format!(
                    "Copilot tier detection: bearer={}ms, fetch_models={}ms, total={}ms, failed to fetch models: {}",
                    bearer_start.elapsed().as_millis(),
                    fetch_start.elapsed().as_millis(),
                    detect_start.elapsed().as_millis(),
                    e
                ));
            }
        }
        self.mark_init_done();
    }
}
