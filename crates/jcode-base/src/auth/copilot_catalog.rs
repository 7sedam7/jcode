//! The Copilot model catalog: what `GET /models` returns and what jcode
//! derives from it.
//!
//! Every reachable model, its limits, its capabilities and its prices come
//! from this response. They vary by account and by the OAuth app the token was
//! minted under, so nothing here may be replaced with a static table.

use super::{COPILOT_AUTH_API_VERSION, EDITOR_VERSION};
use crate::auth::copilot_enterprise::api_base;
use anyhow::{Context, Result};
use jcode_provider_core::copilot_catalog_pricing::{CopilotCatalogBilling, CopilotTokenPricesTier};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{LazyLock, RwLock};

/// Copilot account type - determines API base URL and available models
#[derive(Debug, Clone, PartialEq)]
pub enum CopilotAccountType {
    Individual,
    Business,
    Enterprise,
    Unknown,
}

impl std::fmt::Display for CopilotAccountType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CopilotAccountType::Individual => write!(f, "individual"),
            CopilotAccountType::Business => write!(f, "business"),
            CopilotAccountType::Enterprise => write!(f, "enterprise"),
            CopilotAccountType::Unknown => write!(f, "unknown"),
        }
    }
}

/// Information about the user's Copilot subscription
#[derive(Debug, Clone)]
pub struct CopilotSubscriptionInfo {
    pub account_type: CopilotAccountType,
    pub available_models: Vec<CopilotModelInfo>,
}

/// Model info from the Copilot /models endpoint
#[derive(Debug, Clone, Deserialize)]
pub struct CopilotModelInfo {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub vendor: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub model_picker_enabled: bool,
    #[serde(default)]
    pub capabilities: Option<CopilotModelCapabilities>,
    /// Wire protocols this model serves, e.g. `/v1/messages`, `/responses`,
    /// `/chat/completions`. Empty means the model predates the field and only
    /// serves `/chat/completions`.
    #[serde(default)]
    pub supported_endpoints: Vec<String>,
    #[serde(default)]
    pub policy: Option<CopilotModelPolicy>,
    /// Per-token prices, present only when the request sends a recent
    /// `X-GitHub-Api-Version`. Older versions omit it for every model, which is
    /// why jcode could not price Copilot from the catalog before.
    #[serde(default)]
    pub billing: Option<CopilotModelBilling>,
}

/// Billing block from a `/models` entry.
#[derive(Debug, Clone, Deserialize)]
pub struct CopilotModelBilling {
    #[serde(default)]
    pub token_prices: Option<CopilotTokenPrices>,
}

/// Prices in AIC (Artificial Intelligence Credits) per `batch_size` tokens.
#[derive(Debug, Clone, Deserialize)]
pub struct CopilotTokenPrices {
    #[serde(default)]
    pub batch_size: u64,
    #[serde(default)]
    pub default: Option<CopilotTokenPriceTier>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CopilotTokenPriceTier {
    #[serde(default)]
    pub input_price: u64,
    #[serde(default)]
    pub output_price: u64,
    #[serde(default)]
    pub cache_price: u64,
}

/// Per-model enablement policy. `state != "enabled"` means the user must opt in
/// via GitHub settings before the model will answer.
#[derive(Debug, Clone, Deserialize)]
pub struct CopilotModelPolicy {
    #[serde(default)]
    pub state: String,
}

/// Wire protocol a Copilot model speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CopilotEndpoint {
    /// Anthropic-native Messages API.
    Messages,
    /// OpenAI Responses API.
    Responses,
    /// OpenAI Chat Completions API.
    ChatCompletions,
}

impl CopilotEndpoint {
    /// Path appended to the Copilot API base URL.
    pub fn path(self) -> &'static str {
        match self {
            CopilotEndpoint::Messages => "/v1/messages",
            CopilotEndpoint::Responses => "/responses",
            CopilotEndpoint::ChatCompletions => "/chat/completions",
        }
    }

    pub fn as_str(self) -> &'static str {
        self.path()
    }
}

impl CopilotModelInfo {
    /// The wire protocol to use for this model.
    ///
    /// The order models are advertised in is not a priority signal — the same
    /// Claude family is served with `/chat/completions` and `/v1/messages` in
    /// either order — so preference is applied explicitly. Models advertising
    /// nothing predate the field and only speak `/chat/completions`.
    ///
    /// `ws:` variants are websocket transports and are ignored.
    pub fn endpoint(&self) -> CopilotEndpoint {
        let advertised: Vec<&str> = self
            .supported_endpoints
            .iter()
            .map(|value| value.as_str())
            .filter(|value| !value.starts_with("ws:"))
            .collect();

        if advertised.is_empty() {
            return CopilotEndpoint::ChatCompletions;
        }
        for candidate in [
            CopilotEndpoint::Messages,
            CopilotEndpoint::Responses,
            CopilotEndpoint::ChatCompletions,
        ] {
            if advertised.contains(&candidate.path()) {
                return candidate;
            }
        }
        CopilotEndpoint::ChatCompletions
    }

    /// Context window advertised by the API, if any.
    ///
    /// Falls back to `max_prompt_tokens` the way OpenCode does: a few models
    /// advertise only the prompt cap, and treating those as "unknown" would
    /// drop jcode back to a guessed limit.
    pub fn max_context_window_tokens(&self) -> Option<usize> {
        let limits = self.capabilities.as_ref()?.limits.as_ref()?;
        limits
            .max_context_window_tokens
            .or(limits.max_prompt_tokens)
    }

    /// Output cap advertised by the API, if any.
    pub fn max_output_tokens(&self) -> Option<usize> {
        self.capabilities
            .as_ref()?
            .limits
            .as_ref()?
            .max_output_tokens
    }

    /// Whether the model is disabled by policy for this account.
    pub fn is_disabled_by_policy(&self) -> bool {
        self.policy
            .as_ref()
            .is_some_and(|policy| policy.state.eq_ignore_ascii_case("disabled"))
    }

    /// Largest prompt the model accepts, if advertised.
    pub fn max_prompt_tokens(&self) -> Option<usize> {
        self.capabilities
            .as_ref()?
            .limits
            .as_ref()?
            .max_prompt_tokens
    }

    /// Reasoning-effort values this model accepts, straight from the catalog.
    ///
    /// Every reasoning model advertises its own set — `claude-sonnet-4.6` takes
    /// `low|medium|high|max` while `gpt-5.6-sol` also takes `none` and `xhigh`.
    /// Guessing gets one model right and silently drops the setting elsewhere.
    pub fn reasoning_efforts(&self) -> &[String] {
        self.capabilities
            .as_ref()
            .and_then(|c| c.supports.as_ref())
            .map(|s| s.reasoning_effort.as_slice())
            .unwrap_or_default()
    }

    pub fn supports_reasoning_effort(&self) -> bool {
        !self.reasoning_efforts().is_empty()
    }

    /// Whether the model can call tools. Absent means "not advertised", which
    /// [`Self::is_usable_for_chat`] treats as unusable.
    pub fn supports_tool_calls(&self) -> Option<bool> {
        self.capabilities.as_ref()?.supports.as_ref()?.tool_calls
    }

    /// Whether the model accepts images, by either the explicit `vision` flag
    /// or an advertised image media type.
    pub fn supports_vision(&self) -> bool {
        let Some(capabilities) = self.capabilities.as_ref() else {
            return false;
        };
        if capabilities
            .supports
            .as_ref()
            .and_then(|s| s.vision)
            .unwrap_or(false)
        {
            return true;
        }
        capabilities
            .limits
            .as_ref()
            .and_then(|l| l.vision.as_ref())
            .is_some_and(|vision| {
                vision
                    .supported_media_types
                    .iter()
                    .any(|media| media.starts_with("image/"))
            })
    }

    /// Whether this model can serve an agent turn.
    ///
    /// Mirrors OpenCode's `usable()` filter. The catalog also lists embedding
    /// models and other non-chat entries (`text-embedding-3-small`,
    /// `gpt-41-copilot`); they advertise no token limits and no tool support,
    /// and offering them as chat models produces confusing failures.
    pub fn is_usable_for_chat(&self) -> bool {
        !self.is_disabled_by_policy()
            && self.max_output_tokens().is_some()
            && self.max_prompt_tokens().is_some()
            && self.supports_tool_calls().is_some()
    }

    /// Per-token prices for this model, converted to the shape the pricing
    /// layer consumes. `None` when the catalog omitted billing (older
    /// `X-GitHub-Api-Version`), which makes pricing fall back to the
    /// subscription heuristic.
    pub fn catalog_billing(&self) -> Option<CopilotCatalogBilling> {
        let prices = self.billing.as_ref()?.token_prices.as_ref()?;
        let tier = prices.default.as_ref()?;
        Some(CopilotCatalogBilling {
            batch_size: prices.batch_size,
            default: CopilotTokenPricesTier {
                input_price: tier.input_price,
                output_price: tier.output_price,
                cache_price: tier.cache_price,
            },
        })
    }
}

/// Prices from the last fetched catalog, keyed by model id.
///
/// Pricing is resolved from a free function far from the provider, so the
/// catalog's prices are published here when it lands rather than threaded
/// through every call site.
static CATALOG_BILLING: LazyLock<RwLock<HashMap<String, CopilotCatalogBilling>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Publish the prices from a freshly fetched catalog.
pub fn record_catalog_billing(models: &[CopilotModelInfo]) {
    let prices: HashMap<String, CopilotCatalogBilling> = models
        .iter()
        .filter_map(|m| Some((m.id.clone(), m.catalog_billing()?)))
        .collect();
    if prices.is_empty() {
        return;
    }
    if let Ok(mut cache) = CATALOG_BILLING.write() {
        *cache = prices;
    }
}

/// Prices for `model` from the live catalog, if it has been fetched.
pub fn catalog_billing_for(model: &str) -> Option<CopilotCatalogBilling> {
    CATALOG_BILLING.read().ok()?.get(model).cloned()
}

#[derive(Debug, Clone, Deserialize)]
pub struct CopilotModelCapabilities {
    #[serde(default)]
    pub limits: Option<CopilotModelLimits>,
    #[serde(default)]
    pub supports: Option<CopilotModelSupports>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CopilotModelSupports {
    #[serde(default)]
    pub tool_calls: Option<bool>,
    #[serde(default)]
    pub vision: Option<bool>,
    #[serde(default)]
    pub streaming: Option<bool>,
    #[serde(default)]
    pub structured_outputs: Option<bool>,
    #[serde(default)]
    pub parallel_tool_calls: Option<bool>,
    #[serde(default)]
    pub reasoning_effort: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CopilotModelLimits {
    #[serde(default)]
    pub max_context_window_tokens: Option<usize>,
    #[serde(default)]
    pub max_output_tokens: Option<usize>,
    #[serde(default)]
    pub max_prompt_tokens: Option<usize>,
    #[serde(default)]
    pub vision: Option<CopilotVisionLimits>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CopilotVisionLimits {
    #[serde(default)]
    pub supported_media_types: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ModelsResponse {
    #[serde(default)]
    data: Vec<CopilotModelInfo>,
}

/// Fetch available models from the Copilot API.
pub async fn fetch_available_models(
    client: &reqwest::Client,
    bearer_token: &str,
) -> Result<Vec<CopilotModelInfo>> {
    let resp = client
        .get(format!("{}/models", api_base()))
        .header("Authorization", format!("Bearer {}", bearer_token))
        .header("Editor-Version", EDITOR_VERSION)
        .header("X-GitHub-Api-Version", COPILOT_AUTH_API_VERSION)
        .send()
        .await
        .context("Failed to fetch Copilot models")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = crate::util::http_error_body(resp, "HTTP error").await;
        anyhow::bail!("Copilot models fetch failed (HTTP {}): {}", status, body);
    }

    let models_resp: ModelsResponse = resp
        .json()
        .await
        .context("Failed to parse Copilot models response")?;

    Ok(models_resp.data)
}

/// Models we prefer as the default, best first.
///
/// This is only a preference ordering. A model is never chosen unless the
/// account actually offers it: the set of models a token can reach depends on
/// the OAuth app it was minted under, so any hardcoded id may be absent.
const PREFERRED_DEFAULT_MODELS: &[&str] = &[
    "claude-opus-4.6",
    "claude-sonnet-4.6",
    "claude-opus-4.5",
    "claude-sonnet-4.5",
    "claude-sonnet-4",
    "gpt-5.5",
];

/// Pick the default model, restricted to what this account can actually reach.
///
/// Returning a model that is absent from the catalog earns an HTTP 400
/// (`model_not_available_for_integrator`) on the very first request, so the
/// catalog is authoritative and the preference list is only a ranking.
pub fn choose_default_model(available_models: &[CopilotModelInfo]) -> String {
    let usable = |m: &&CopilotModelInfo| !m.is_disabled_by_policy();

    for preferred in PREFERRED_DEFAULT_MODELS {
        if available_models
            .iter()
            .filter(usable)
            .any(|m| m.id == *preferred)
        {
            return (*preferred).to_string();
        }
    }

    // No preferred model is available. Rather than send a request we know will
    // fail, take what the account does offer, favouring models GitHub marks as
    // user-selectable.
    available_models
        .iter()
        .filter(usable)
        .find(|m| m.model_picker_enabled)
        .or_else(|| available_models.iter().find(usable))
        .map(|m| m.id.clone())
        // Only reachable when the catalog is empty (offline start); the caller
        // keeps whatever model it already had in that case.
        .unwrap_or_else(|| DEFAULT_FALLBACK_MODEL.to_string())
}

/// Used only when the catalog could not be read at all.
const DEFAULT_FALLBACK_MODEL: &str = "claude-sonnet-4";
