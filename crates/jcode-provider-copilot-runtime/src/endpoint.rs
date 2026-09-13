//! Copilot credential and seat-endpoint lifecycle.

use super::*;

impl CopilotApiProvider {
    pub(super) fn reload_credentials_now(&self) {
        self.invalidate_credential_generation();
        if let Err(error) = self.reload_github_token() {
            jcode_base::logging::warn(&format!("Failed to reload Copilot credentials: {error}"));
            self.clear_github_token();
        }
    }

    pub(super) async fn invalidate_credentials_and_catalog(&self) {
        self.reload_credentials_now();
        if let Ok(token) = self.get_bearer_token().await {
            // Re-login may change entitlements without changing the OAuth token.
            copilot_auth_enterprise::clear_discovered_api_base_for(&token);
        }
        // The OAuth app also controls the model catalog, so force a fresh list.
        self.clear_catalog();
    }

    pub(super) fn credential_is_current(&self, candidate: &str, generation: u64) -> bool {
        let token = self
            .github_token
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        token.as_str() == candidate
            && self
                .credential_generation
                .load(std::sync::atomic::Ordering::Acquire)
                == generation
    }

    /// Copilot accepts the GitHub OAuth token directly, so there is no exchange
    /// or short-lived bearer cache to refresh.
    pub(super) async fn get_bearer_token(&self) -> Result<String> {
        let token = self
            .github_token
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if token.is_empty() {
            anyhow::bail!(
                "GitHub Copilot credentials are no longer available. Run `jcode login --provider copilot`."
            );
        }
        Ok(token)
    }

    /// Apply account-derived state only while the credential snapshot is still
    /// current. The generation also changes for a same-token re-login, where the
    /// seat entitlements may have changed even though token equality has not.
    pub(super) fn with_current_credential<T>(
        &self,
        candidate: &str,
        generation: u64,
        apply: impl FnOnce() -> T,
    ) -> Option<T> {
        let token = self
            .github_token
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if token.as_str() != candidate
            || self
                .credential_generation
                .load(std::sync::atomic::Ordering::Acquire)
                != generation
        {
            return None;
        }
        Some(apply())
    }

    pub(super) fn credential_generation(&self) -> u64 {
        self.credential_generation
            .load(std::sync::atomic::Ordering::Acquire)
    }

    pub(super) fn invalidate_credential_generation(&self) {
        self.credential_generation
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    }

    pub(super) fn reload_github_token(&self) -> Result<()> {
        let fresh = copilot_auth::load_github_token()?;
        let mut token = self
            .github_token
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *token != fresh {
            copilot_auth_enterprise::clear_discovered_api_base_for(&token);
            *token = fresh;
        }
        Ok(())
    }

    pub(super) fn clear_github_token(&self) {
        let mut token = self
            .github_token
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        copilot_auth_enterprise::clear_discovered_api_base_for(&token);
        token.clear();
    }

    pub(super) fn clear_catalog(&self) {
        if let Ok(mut models) = self.fetched_models.write() {
            models.clear();
        }
        if let Ok(mut specs) = self.model_specs.write() {
            *specs = CatalogSpecs::default();
        }
        if let Ok(mut source) = self.catalog_source.write() {
            *source = CatalogSource::None;
        }
        if let Ok(mut account) = self.account_type.write() {
            *account = copilot_auth::CopilotAccountType::Unknown;
        }
        jcode_provider_core::clear_copilot_catalog_context_limits();
        copilot_auth::clear_catalog_billing();
    }

    /// GitHub also returns 403 when an Enterprise seat is sent to the public
    /// inference endpoint, so only 401 is safe to label as failed auth.
    pub(super) fn is_auth_error(status: reqwest::StatusCode) -> bool {
        status == reqwest::StatusCode::UNAUTHORIZED
    }

    /// Resolve the seat endpoint before capturing the inference URL.
    pub(super) async fn ensure_request_api_base(&self, bearer_token: &str) -> String {
        match copilot_auth_enterprise::ensure_api_base(&self.client, bearer_token).await {
            Ok(base) => base,
            Err(error) => {
                jcode_base::logging::warn(&format!(
                    "Could not discover the Copilot seat endpoint before inference ({error}); \
                     falling back to the configured deployment endpoint"
                ));
                copilot_auth_enterprise::api_base_for(bearer_token)
            }
        }
    }

    /// Rediscover after a 403, returning a value only when GitHub moved the
    /// credential to a different inference host.
    pub(super) async fn refresh_api_base_after_forbidden(
        &self,
        bearer_token: &str,
        previous_base: &str,
    ) -> Option<String> {
        copilot_auth_enterprise::clear_discovered_api_base_for(bearer_token);
        let refreshed =
            match copilot_auth_enterprise::ensure_api_base(&self.client, bearer_token).await {
                Ok(base) => base,
                Err(error) => {
                    jcode_base::logging::warn(&format!(
                        "Could not rediscover the Copilot endpoint after HTTP 403: {error}"
                    ));
                    return None;
                }
            };
        (refreshed != previous_base).then_some(refreshed)
    }
}
