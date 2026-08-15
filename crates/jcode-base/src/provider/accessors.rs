use super::*;

impl MultiProvider {
    pub(super) fn claude_provider(&self) -> Option<Arc<dyn Provider>> {
        self.claude
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub(super) fn anthropic_provider(&self) -> Option<Arc<dyn Provider>> {
        self.anthropic
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub(super) fn openai_provider(&self) -> Option<Arc<dyn Provider>> {
        self.openai
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub(super) fn antigravity_provider(&self) -> Option<Arc<dyn Provider>> {
        self.antigravity
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub(super) fn gemini_provider(&self) -> Option<Arc<dyn Provider>> {
        self.gemini
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub(super) fn copilot_provider(&self) -> Option<Arc<dyn Provider>> {
        self.copilot_api
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub(super) fn cursor_provider(&self) -> Option<Arc<dyn Provider>> {
        self.cursor
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub(super) fn bedrock_provider(&self) -> Option<Arc<bedrock::BedrockProvider>> {
        self.bedrock
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub(super) fn openrouter_provider(&self) -> Option<Arc<dyn Provider>> {
        ProviderRegistry::new(self).real_openrouter()
    }

    pub(super) fn active_openrouter_execution_provider(&self) -> Option<Arc<dyn Provider>> {
        ProviderRegistry::new(self).active_openrouter_execution()
    }

    pub(super) fn clear_active_openai_compatible_profile(&self) {
        ProviderRegistry::new(self).clear_active_compatible_profile();
    }

    pub(super) fn has_claude_runtime(&self) -> bool {
        self.anthropic_provider().is_some() || self.claude_provider().is_some()
    }

    pub(super) fn provider_slot_available(&self, provider: ActiveProvider) -> bool {
        match provider {
            ActiveProvider::Claude => self.has_claude_runtime(),
            ActiveProvider::OpenAI => self.openai_provider().is_some(),
            ActiveProvider::Copilot => self.copilot_provider().is_some(),
            ActiveProvider::Antigravity => self.antigravity_provider().is_some(),
            ActiveProvider::Gemini => self.gemini_provider().is_some(),
            ActiveProvider::Cursor => self.cursor_provider().is_some(),
            ActiveProvider::Bedrock => self.bedrock_provider().is_some(),
            // The OpenRouter slot executes through the *active* runtime: a
            // direct OpenAI-compatible profile when one is active, else real
            // OpenRouter. Checking only the real slot here made dispatch treat
            // an active compat profile (e.g. minimax) as "not configured"
            // whenever no OPENROUTER_API_KEY existed, and the failover loop
            // then silently rerouted the request to another provider such as
            // OpenAI (issue #358).
            ActiveProvider::OpenRouter => self.active_openrouter_execution_provider().is_some(),
        }
    }

    pub(super) fn reconcile_auth_if_provider_missing(&self, provider: ActiveProvider) -> bool {
        if self.provider_slot_available(provider) {
            return true;
        }

        crate::logging::info(&format!(
            "Provider {} missing at use site; reconciling auth from disk",
            Self::provider_label(provider)
        ));
        Provider::on_auth_changed(self);
        self.provider_slot_available(provider)
    }
}

impl MultiProvider {
    /// Route an unprefixed model to Copilot when this seat's catalog lists it.
    ///
    /// A Copilot seat serves upstream model ids verbatim (`gpt-5.5`,
    /// `gemini-3.6-flash`, `claude-opus-5`), so the global name heuristics send
    /// them to OpenAI, Gemini or Anthropic. Those are usually not authenticated:
    /// the switch fails and Copilot silently keeps serving its previous model,
    /// which made every pick but the catalog default look broken. Only the
    /// account's own catalog can settle who owns a bare name.
    ///
    /// Returns `None` when the decision does not apply, so the caller continues
    /// with normal routing. Explicit `<provider>:` prefixes are resolved before
    /// this runs and still win, and an empty catalog (credentials present but
    /// `/models` not answered yet) claims nothing.
    pub(super) fn set_model_if_copilot_catalog_owns(
        &self,
        requested_model: &str,
    ) -> Option<Result<()>> {
        if self.active_provider() != ActiveProvider::Copilot {
            return None;
        }
        let model = requested_model.trim();
        if model.is_empty() {
            return None;
        }
        let copilot = self.copilot_provider()?;
        copilot_catalog_lists(copilot.as_ref(), model)
            .then(|| self.set_model_on_provider(ActiveProvider::Copilot, requested_model))
    }
}

/// Whether a Copilot seat's live catalog lists `model`.
pub(super) fn copilot_catalog_lists(copilot: &dyn Provider, model: &str) -> bool {
    let model = model.trim();
    if model.is_empty() {
        return false;
    }
    copilot
        .available_models_for_switching()
        .iter()
        .chain(copilot.available_models_display().iter())
        .any(|listed| listed.trim().eq_ignore_ascii_case(model))
}
