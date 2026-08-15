//! GitHub Copilot provider runtime (direct API with the user's GitHub token as
//! bearer, tier detection, premium request modes), moved out of `jcode-base` so
//! provider edits compile only this crate plus a binary relink instead of
//! rebuilding the base -> app-core -> tui spine. The binary's composition
//! root registers [`CopilotApiProvider`] with `jcode_base::provider::external`
//! at startup.

mod catalog;
mod errors;
mod messages;
mod responses;
mod startup;
mod stream_chat;
mod stream_messages;
mod stream_responses;

/// Exposed for the opt-in live integration test, which exercises the real
/// request builder rather than a hand-written copy of it.
pub mod testing {
    pub use crate::messages::ANTHROPIC_BETA;

    /// Build a `/v1/messages` body for a single-user-turn probe.
    pub fn build_messages_request(
        model: &str,
        system: &str,
        prompt: &str,
        max_tokens: u32,
    ) -> serde_json::Value {
        let message = jcode_message_types::Message {
            role: jcode_message_types::Role::User,
            content: vec![jcode_message_types::ContentBlock::Text {
                text: prompt.to_string(),
                cache_control: None,
            }],
            timestamp: None,
            tool_duration_ms: None,
        };
        crate::messages::build_request(model, system, &[message], &[], max_tokens, None)
    }
}

use anyhow::Result;
use async_trait::async_trait;
use catalog::CatalogSpecs;
use chrono::Utc;
use jcode_base::auth::copilot as copilot_auth;
use jcode_base::auth::copilot_enterprise as copilot_auth_enterprise;
use jcode_message_types::{
    ContentBlock, Message as ChatMessage, Role, StreamEvent, ToolDefinition,
};
use jcode_provider_copilot::DEFAULT_MODEL;
#[cfg(test)]
use jcode_provider_copilot::max_token_parameter_for_model as copilot_max_token_parameter_for_model;
use jcode_provider_copilot::{
    COPILOT_API_VERSION, add_max_token_parameter as add_copilot_max_token_parameter,
    build_messages as build_copilot_messages, build_tools as build_copilot_tools,
};
pub use jcode_provider_core::PremiumMode;
use jcode_provider_core::{EventStream, Provider};
use serde_json::{Value, json};
use std::sync::{Arc, RwLock};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CatalogSource {
    None,
    Cached,
    Live,
}

/// Copilot API provider - uses GitHub Copilot's OpenAI-compatible API.
/// Authenticates via GitHub OAuth token, exchanges for Copilot bearer token,
/// and sends requests to api.githubcopilot.com.
pub struct CopilotApiProvider {
    client: reqwest::Client,
    model: Arc<RwLock<String>>,
    github_token: String,
    fetched_models: Arc<RwLock<Vec<String>>>,
    /// Per-model endpoint + limits, as last reported by `/models`.
    model_specs: Arc<RwLock<CatalogSpecs>>,
    catalog_source: Arc<RwLock<CatalogSource>>,
    /// Plan GitHub reports for this seat, once the seat lookup answers.
    account_type: Arc<RwLock<copilot_auth::CopilotAccountType>>,
    session_id: String,
    machine_id: String,
    init_ready: Arc<tokio::sync::Notify>,
    init_done: Arc<std::sync::atomic::AtomicBool>,
    premium_mode: Arc<std::sync::atomic::AtomicU8>,
    user_turn_count: Arc<std::sync::atomic::AtomicU64>,
    reasoning_effort: Arc<RwLock<Option<String>>>,
    /// Set once a caller picks a model explicitly (`--model`, the `/model`
    /// picker, session restore). Tier detection runs asynchronously and lands
    /// after that choice, so without this it would overwrite the selection with
    /// the catalog default and silently send every turn to the wrong model.
    model_explicitly_selected: Arc<std::sync::atomic::AtomicBool>,
    created_at: std::time::Instant,
}

/// The reasoning-effort names Copilot uses, as `&'static str`.
///
/// [`Provider::available_efforts`] hands back `&'static str`, but the authority
/// on which efforts a model accepts is the live catalog, whose strings are
/// owned. Interning the known vocabulary bridges the two without leaking a new
/// allocation on every catalog refresh. An effort GitHub adds later is still
/// accepted by `set_reasoning_effort` (which validates against the catalog); it
/// just will not appear in the picker until it is listed here.
const KNOWN_EFFORTS: [&str; 7] = ["none", "minimal", "low", "medium", "high", "xhigh", "max"];

/// Declares what the request is for. Copilot uses it for quota attribution, and
/// this is the value a coding agent sends.
const OPENAI_INTENT: &str = "conversation-edits";

fn intern_effort(effort: &str) -> Option<&'static str> {
    KNOWN_EFFORTS
        .iter()
        .find(|known| known.eq_ignore_ascii_case(effort))
        .copied()
}

/// Base URL for Copilot API requests.
///
/// Prefers the endpoint GitHub assigned this seat (enterprise seats are served
/// from `api.enterprise.githubcopilot.com`), falling back to the configured
/// deployment's default until discovery answers.
fn request_base_url() -> String {
    copilot_auth_enterprise::api_base()
}

impl CopilotApiProvider {
    /// Reasoning efforts the active model accepts, per the live catalog.
    fn efforts_for(&self, model: &str) -> Vec<String> {
        self.model_specs
            .read()
            .map(|specs| specs.reasoning_efforts_for(model).to_vec())
            .unwrap_or_default()
    }

    /// Whether the live catalog says this model accepts image input.
    fn model_supports_vision(&self, model: &str) -> bool {
        self.model_specs
            .read()
            .map(|specs| specs.supports_vision(model))
            .unwrap_or(false)
    }

    fn model_supports_reasoning_effort(&self, model: &str) -> bool {
        !self.efforts_for(model).is_empty()
    }

    #[cfg(test)]
    fn max_token_parameter_for_model(model: &str) -> &'static str {
        copilot_max_token_parameter_for_model(model)
    }

    fn add_max_token_parameter(body: &mut Value, model: &str, max_tokens: u32) {
        add_copilot_max_token_parameter(body, model, max_tokens);
    }

    fn current_reasoning_effort(&self) -> Option<String> {
        self.reasoning_effort
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// The reasoning effort to send for `model`, or `None` when the user has
    /// not chosen one or the model does not support it.
    fn reasoning_effort_for(&self, model: &str) -> Option<String> {
        if !self.model_supports_reasoning_effort(model) {
            return None;
        }
        self.current_reasoning_effort()
    }

    /// Add top-level `reasoning_effort` when set and the model supports it.
    fn add_reasoning_effort_parameter(&self, body: &mut Value, model: &str) {
        if !self.model_supports_reasoning_effort(model) {
            return;
        }
        if let Some(effort) = self.current_reasoning_effort() {
            body["reasoning_effort"] = json!(effort);
        }
    }

    /// Whether any catalog (live or cached) is loaded.
    fn has_catalog(&self) -> bool {
        self.fetched_models
            .read()
            .map(|models| !models.is_empty())
            .unwrap_or(false)
    }

    fn available_model_ids(&self) -> Vec<String> {
        self.fetched_models
            .read()
            .map(|models| models.clone())
            .unwrap_or_default()
    }

    fn write_model(&self, model: String) {
        if let Ok(mut current) = self.model.write() {
            *current = model;
        }
    }

    fn model_catalog_detail_impl(&self) -> String {
        let source = match self
            .catalog_source
            .try_read()
            .map(|g| *g)
            .unwrap_or(CatalogSource::None)
        {
            CatalogSource::Live => String::new(),
            CatalogSource::Cached => "cached live catalog".to_string(),
            CatalogSource::None => "catalog still loading".to_string(),
        };

        // The plan explains why two accounts see different models, which is
        // otherwise the most confusing thing about Copilot.
        let plan = self
            .account_type
            .try_read()
            .map(|plan| plan.to_string())
            .unwrap_or_else(|_| "unknown".to_string());
        match (source.is_empty(), plan.as_str()) {
            (true, "unknown") => String::new(),
            (true, plan) => format!("{plan} plan"),
            (false, "unknown") => source,
            (false, plan) => format!("{source}, {plan} plan"),
        }
    }

    pub fn new() -> Result<Self> {
        let github_token = copilot_auth::load_github_token()?;
        let model =
            std::env::var("JCODE_COPILOT_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());

        let provider = Self {
            client: jcode_provider_core::shared_http_client(),
            model: Arc::new(RwLock::new(model)),
            github_token,
            fetched_models: Arc::new(RwLock::new(Vec::new())),
            model_specs: Arc::new(RwLock::new(CatalogSpecs::default())),
            catalog_source: Arc::new(RwLock::new(CatalogSource::None)),
            account_type: Arc::new(RwLock::new(copilot_auth::CopilotAccountType::Unknown)),
            session_id: Uuid::new_v4().to_string(),
            machine_id: Self::get_or_create_machine_id(),
            init_ready: Arc::new(tokio::sync::Notify::new()),
            init_done: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            premium_mode: Arc::new(std::sync::atomic::AtomicU8::new(Self::env_premium_mode())),
            user_turn_count: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            reasoning_effort: Arc::new(RwLock::new(None)),
            model_explicitly_selected: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            created_at: std::time::Instant::now(),
        };
        provider.seed_cached_catalog();
        Ok(provider)
    }

    pub fn has_credentials() -> bool {
        copilot_auth::has_copilot_credentials()
    }

    fn env_premium_mode() -> u8 {
        match std::env::var("JCODE_COPILOT_PREMIUM").ok().as_deref() {
            Some("0") => PremiumMode::Zero as u8,
            Some("1") => PremiumMode::OnePerSession as u8,
            _ => PremiumMode::Normal as u8,
        }
    }

    pub fn new_with_token(github_token: String) -> Self {
        let model =
            std::env::var("JCODE_COPILOT_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());

        let provider = Self {
            client: jcode_provider_core::shared_http_client(),
            model: Arc::new(RwLock::new(model)),
            github_token,
            fetched_models: Arc::new(RwLock::new(Vec::new())),
            model_specs: Arc::new(RwLock::new(CatalogSpecs::default())),
            catalog_source: Arc::new(RwLock::new(CatalogSource::None)),
            account_type: Arc::new(RwLock::new(copilot_auth::CopilotAccountType::Unknown)),
            session_id: Uuid::new_v4().to_string(),
            machine_id: Self::get_or_create_machine_id(),
            init_ready: Arc::new(tokio::sync::Notify::new()),
            init_done: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            premium_mode: Arc::new(std::sync::atomic::AtomicU8::new(Self::env_premium_mode())),
            user_turn_count: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            reasoning_effort: Arc::new(RwLock::new(None)),
            model_explicitly_selected: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            created_at: std::time::Instant::now(),
        };
        provider.seed_cached_catalog();
        provider
    }

    fn startup_prefetch_grace_ms() -> u64 {
        std::env::var("JCODE_COPILOT_PREFETCH_STARTUP_GRACE_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(2000)
    }

    fn get_or_create_machine_id() -> String {
        let machine_id_path = dirs::home_dir()
            .unwrap_or_default()
            .join(".jcode")
            .join("machine_id");
        if let Ok(id) = std::fs::read_to_string(&machine_id_path) {
            let id = id.trim().to_string();
            if !id.is_empty() {
                return id;
            }
        }
        let id = Uuid::new_v4().to_string().replace('-', "");
        let _ = std::fs::create_dir_all(machine_id_path.parent().unwrap_or(&machine_id_path));
        let _ = std::fs::write(&machine_id_path, &id);
        id
    }

    fn is_user_initiated_raw(messages: &[ChatMessage]) -> bool {
        for msg in messages.iter().rev() {
            if msg.role != Role::User {
                return true;
            }
            let has_tool_result = msg
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolResult { .. }));
            if has_tool_result {
                return false;
            }
            let is_text_only = msg
                .content
                .iter()
                .all(|block| matches!(block, ContentBlock::Text { .. }));
            if !is_text_only || msg.content.is_empty() {
                return true;
            }
            let is_system_reminder = msg.content.iter().any(|block| {
                if let ContentBlock::Text { text, .. } = block {
                    text.contains("<system-reminder>")
                } else {
                    false
                }
            });
            if is_system_reminder {
                continue;
            }
            return true;
        }
        true
    }

    fn is_user_initiated(&self, messages: &[ChatMessage]) -> bool {
        let raw = Self::is_user_initiated_raw(messages);
        if !raw {
            return false;
        }
        let mode = self.premium_mode.load(std::sync::atomic::Ordering::Relaxed);
        match mode {
            2 => false,
            1 => {
                let count = self
                    .user_turn_count
                    .load(std::sync::atomic::Ordering::Relaxed);
                count == 0
            }
            _ => true,
        }
    }

    pub fn set_premium_mode(&self, mode: PremiumMode) {
        self.premium_mode
            .store(mode as u8, std::sync::atomic::Ordering::Relaxed);
        if mode != PremiumMode::Normal {
            jcode_base::logging::info(&format!("Copilot premium mode set to {:?}", mode));
        }
    }

    pub fn get_premium_mode(&self) -> PremiumMode {
        match self.premium_mode.load(std::sync::atomic::Ordering::Relaxed) {
            1 => PremiumMode::OnePerSession,
            2 => PremiumMode::Zero,
            _ => PremiumMode::Normal,
        }
    }

    /// Detect the user's Copilot tier and set the best default model.
    /// Call this after construction. Fetches a bearer token and queries /models.
    /// If JCODE_COPILOT_MODEL is set, this is a no-op (user override).
    fn mark_init_done(&self) {
        self.init_done
            .store(true, std::sync::atomic::Ordering::Release);
        self.init_ready.notify_waiters();
        jcode_base::bus::Bus::global().publish_models_updated();
    }

    pub fn complete_init_without_tier_detection(&self) {
        self.mark_init_done();
    }

    async fn wait_for_init(&self) {
        if self.init_done.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        let notified = self.init_ready.notified();
        if self.init_done.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        notified.await;
    }

    /// The bearer token for Copilot API calls.
    ///
    /// Copilot accepts the user's GitHub OAuth token directly, so there is no
    /// exchange step, nothing cached, and no expiry to track. A token that stops
    /// working means the user must re-authenticate.
    async fn get_bearer_token(&self) -> Result<String> {
        Ok(self.github_token.clone())
    }

    /// Check if an error indicates token expiration
    fn is_auth_error(status: reqwest::StatusCode) -> bool {
        status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN
    }

    /// Build OpenAI-compatible messages array from our message format.
    fn build_messages(system: &str, messages: &[ChatMessage]) -> Vec<Value> {
        build_copilot_messages(system, messages)
    }

    /// Build OpenAI-compatible tools array.
    fn build_tools(tools: &[ToolDefinition]) -> Vec<Value> {
        build_copilot_tools(tools)
    }

    /// Send a streaming request to Copilot API with retry logic
    async fn stream_request(
        &self,
        messages: Vec<Value>,
        tools: Vec<Value>,
        raw: RawTurn,
        is_user_initiated: bool,
        tx: mpsc::Sender<Result<StreamEvent>>,
    ) {
        use jcode_message_types::ConnectionPhase;

        self.wait_for_init().await;
        let model = self
            .model
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let max_tokens: u32 = 32_768;
        let initiator = if is_user_initiated { "user" } else { "agent" };
        let has_images = raw.has_images();
        // A model that does not advertise vision returns an opaque 400 for image
        // parts, so say plainly what happened instead of letting it look like a
        // transport fault.
        if has_images && !self.model_supports_vision(&model) {
            jcode_base::logging::warn(&format!(
                "Copilot model '{model}' does not advertise vision support; \
                 the attached image will likely be rejected. Switch models with /model."
            ));
        }
        let api_base = request_base_url();

        const MAX_RETRIES: u32 = 3;
        const RETRY_BASE_DELAY_MS: u64 = 1000;
        let mut last_error: Option<anyhow::Error> = None;

        for attempt in 0..MAX_RETRIES {
            if attempt > 0 {
                let delay = jcode_provider_core::attempt_tracker::retry_backoff_delay(
                    attempt,
                    RETRY_BASE_DELAY_MS,
                );
                jcode_base::logging::info(&format!(
                    "Retrying Copilot API request (attempt {}/{}) after {}ms",
                    attempt + 1,
                    MAX_RETRIES,
                    delay.as_millis()
                ));
                let _ = tx
                    .send(Ok(StreamEvent::ConnectionPhase {
                        phase: ConnectionPhase::Retrying {
                            attempt: attempt + 1,
                            max: MAX_RETRIES,
                        },
                    }))
                    .await;
                tokio::time::sleep(delay).await;
            }

            jcode_base::logging::info(&format!(
                "Copilot request: X-Initiator={} model={}",
                initiator, model
            ));

            let bearer_token = match self.get_bearer_token().await {
                Ok(t) => t,
                Err(e) => {
                    let _ = tx.send(Err(e)).await;
                    return;
                }
            };

            // Copilot rejects a model sent to an endpoint it does not serve
            // (HTTP 400 `unsupported_api_for_model`), so the route comes from
            // the live catalog rather than being assumed.
            //
            // A model the catalog has not described yet is the dangerous case:
            // guessing `/chat/completions` for it produces a hard 400 that no
            // retry can fix. That happens routinely — the daemon starts with
            // tier detection disabled and seeds from a cached catalog, so any
            // model GitHub added since that cache was written is picker-visible
            // but undescribed. Fetch before guessing.
            let endpoint = self.endpoint_for_model(&model, &bearer_token).await;

            // Never ask for more output than the model actually allows: the
            // catalog caps range from 4k (gpt-4o) to 128k (gpt-5.x), and the
            // caller's request is only an upper bound.
            let effective_max_tokens = self
                .model_specs
                .read()
                .ok()
                .and_then(|specs| specs.max_output_tokens_for(&model))
                .map(|cap| max_tokens.min(cap as u32))
                .unwrap_or(max_tokens);

            let body = match endpoint {
                copilot_auth::CopilotEndpoint::Messages => messages::build_request(
                    &model,
                    &raw.system,
                    &raw.messages,
                    &raw.tools,
                    effective_max_tokens,
                    self.reasoning_effort_for(&model).as_deref(),
                ),
                copilot_auth::CopilotEndpoint::Responses => responses::build_request(
                    &model,
                    &raw.system,
                    &raw.messages,
                    &raw.tools,
                    effective_max_tokens,
                    self.reasoning_effort_for(&model).as_deref(),
                ),
                copilot_auth::CopilotEndpoint::ChatCompletions => {
                    let mut body = json!({
                        "model": model,
                        "messages": messages,
                        "stream": true,
                    });
                    Self::add_max_token_parameter(&mut body, &model, effective_max_tokens);
                    self.add_reasoning_effort_parameter(&mut body, &model);
                    if !tools.is_empty() {
                        body["tools"] = json!(tools);
                    }
                    body
                }
            };

            let request_id = Uuid::new_v4().to_string();

            // Retries use a fresh unpooled client: the fault that broke
            // attempt N (e.g. TLS BadRecordMac from a corrupting middlebox)
            // may also have poisoned other idle pooled connections opened
            // through the same path, so reusing the shared pool can fail
            // identically. A fresh client guarantees a new TCP+TLS connection.
            let attempt_client = if attempt == 0 {
                self.client.clone()
            } else {
                jcode_provider_core::fresh_transport_client()
            };

            let req = attempt_client
                .post(format!("{}{}", api_base, endpoint.path()))
                .header("Authorization", format!("Bearer {}", bearer_token))
                .header("Content-Type", "application/json")
                .header("X-Initiator", initiator)
                .header("X-Request-Id", &request_id)
                .header("X-GitHub-Api-Version", COPILOT_API_VERSION)
                .header("User-Agent", copilot_auth::EDITOR_VERSION)
                .header("Editor-Version", copilot_auth::EDITOR_VERSION)
                .header("Openai-Intent", OPENAI_INTENT);

            // Copilot rejects image parts unless the request opts in, so a turn
            // carrying an attachment fails without this even on a vision model.
            let req = if has_images {
                req.header("Copilot-Vision-Request", "true")
            } else {
                req
            };

            // Interleaved thinking keeps reasoning coherent between tool calls
            // on the Messages route; it is meaningless on the others.
            let req = if endpoint == copilot_auth::CopilotEndpoint::Messages {
                req.header("anthropic-beta", messages::ANTHROPIC_BETA)
            } else {
                req
            };

            let resp = req.json(&body).send().await;

            let resp = match resp {
                Ok(r) => r,
                Err(e) => {
                    // Full anyhow chain ({:#}) so a `.context(...)`-wrapped
                    // transport cause (e.g. TLS BadRecordMac) is visible to the
                    // retry classifier.
                    let error_str = format!("{e:#}").to_lowercase();
                    if is_retryable_error(&error_str) && attempt + 1 < MAX_RETRIES {
                        jcode_base::logging::info(&format!(
                            "Transient Copilot error, will retry: {}",
                            e
                        ));
                        last_error = Some(anyhow::anyhow!("Copilot API request failed: {}", e));
                        continue;
                    }
                    let _ = tx
                        .send(Err(anyhow::anyhow!("Copilot API request failed: {}", e)))
                        .await;
                    return;
                }
            };

            let status = resp.status();

            // The GitHub token is used directly, so there is nothing to refresh:
            // a 401 means the user must re-authenticate. Retrying would only
            // replay the same rejected credential.
            if Self::is_auth_error(status) {
                let body_text = jcode_base::util::http_error_body(resp, "HTTP error").await;
                let _ = tx
                    .send(Err(anyhow::anyhow!(
                        "Copilot authentication failed (HTTP {status}). Run `jcode login --provider copilot` to re-authenticate: {body_text}"
                    )))
                    .await;
                return;
            }

            if !status.is_success() {
                let body_text = jcode_base::util::http_error_body(resp, "HTTP error").await;
                // A stale spec, rather than a missing one: the catalog described
                // this model with an endpoint it no longer serves, so the miss
                // that `endpoint_for_model` refetches on never fired. Forget the
                // spec and retry — the next attempt refetches and routes right.
                if status == reqwest::StatusCode::BAD_REQUEST
                    && body_text.contains(errors::UNSUPPORTED_API_FOR_MODEL)
                    && attempt + 1 < MAX_RETRIES
                    && self
                        .model_specs
                        .write()
                        .map(|mut specs| specs.forget(&model))
                        .unwrap_or(false)
                {
                    jcode_base::logging::info(&format!(
                        "Copilot rejected {} for '{model}'; the cached catalog entry is stale, \
                         refetching and retrying",
                        endpoint.path()
                    ));
                    last_error = Some(anyhow::anyhow!(
                        "Copilot API error (HTTP {}): {}",
                        status,
                        body_text
                    ));
                    continue;
                }
                let error_str =
                    format!("Copilot API error (HTTP {}): {}", status, body_text).to_lowercase();
                if is_retryable_error(&error_str) && attempt + 1 < MAX_RETRIES {
                    jcode_base::logging::info(&format!(
                        "Retryable Copilot HTTP error: {}",
                        error_str
                    ));
                    last_error = Some(anyhow::anyhow!(
                        "Copilot API error (HTTP {}): {}",
                        status,
                        body_text
                    ));
                    continue;
                }
                let body_text = errors::annotate(status.as_u16(), &body_text);
                let _ = tx
                    .send(Err(anyhow::anyhow!(
                        "Copilot API error (HTTP {}): {}",
                        status,
                        body_text
                    )))
                    .await;
                return;
            }

            // Send connection type event
            let _ = tx
                .send(Ok(StreamEvent::ConnectionType {
                    connection: format!("copilot-api ({})", model),
                }))
                .await;

            // Track whether this attempt streams replay-visible output so a
            // mid-stream transport fault can roll the partial output back on
            // the consumer before the retry replays the response from the top.
            let (attempt_tx, attempt_guard) =
                jcode_provider_core::attempt_tracker::track_attempt_output(tx.clone());

            // Process SSE stream - returns Err on timeout/stream errors
            match self
                .process_stream(endpoint, &model, resp, attempt_tx)
                .await
            {
                Ok(()) => {
                    let _ = attempt_guard.finish().await;
                    return;
                }
                Err(e) => {
                    let saw_output = attempt_guard.finish().await;
                    // Full anyhow chain ({:#}) so a `.context(...)`-wrapped
                    // transport cause (e.g. TLS BadRecordMac) is visible to the
                    // retry classifier.
                    let error_str = format!("{e:#}").to_lowercase();
                    if is_retryable_error(&error_str) && attempt + 1 < MAX_RETRIES {
                        if saw_output {
                            // Partial output already reached the consumer; tell
                            // it to discard the partial attempt so the retried
                            // response replays cleanly instead of duplicating.
                            jcode_base::logging::warn(&format!(
                                "Copilot stream failed after partial output (attempt {}/{}); rolling back partial attempt and retrying: {}",
                                attempt + 1,
                                MAX_RETRIES,
                                e
                            ));
                            let _ = tx
                                .send(Ok(StreamEvent::RetryRollback {
                                    attempt: attempt + 2,
                                    max: MAX_RETRIES,
                                }))
                                .await;
                        } else {
                            jcode_base::logging::info(&format!(
                                "Copilot stream failed (attempt {}/{}), will retry: {}",
                                attempt + 1,
                                MAX_RETRIES,
                                e
                            ));
                        }
                        last_error = Some(e);
                        continue;
                    }
                    let _ = tx.send(Err(e)).await;
                    return;
                }
            }
        }

        // All retries exhausted
        if let Some(e) = last_error {
            let _ = tx
                .send(Err(anyhow::anyhow!(
                    "Copilot: failed after {} retries: {}",
                    MAX_RETRIES,
                    e
                )))
                .await;
        }
    }

    /// Decode a streaming response with the decoder that matches the wire
    /// protocol the model is served over.
    async fn process_stream(
        &self,
        endpoint: copilot_auth::CopilotEndpoint,
        model: &str,
        resp: reqwest::Response,
        tx: mpsc::Sender<Result<StreamEvent>>,
    ) -> Result<()> {
        match endpoint {
            copilot_auth::CopilotEndpoint::Messages => {
                stream_messages::process_messages_sse_stream(resp, model, tx).await
            }
            copilot_auth::CopilotEndpoint::Responses => {
                stream_responses::process_responses_sse_stream(resp, tx).await
            }
            copilot_auth::CopilotEndpoint::ChatCompletions => {
                stream_chat::process_chat_sse_stream(resp, tx).await
            }
        }
    }
}

fn is_retryable_error(error_str: &str) -> bool {
    jcode_provider_core::is_transient_transport_error(error_str)
        || error_str.contains("500 internal server error")
        || error_str.contains("502 bad gateway")
        || error_str.contains("503 service unavailable")
        || error_str.contains("504 gateway timeout")
        || error_str.contains("overloaded")
        || error_str.contains("429 too many requests")
        || error_str.contains("rate limit")
        || error_str.contains("rate_limit")
        || error_str.contains("stream error")
        || error_str.contains("stream read timeout")
}

/// The turn in jcode's own representation.
///
/// Only the Chat Completions route consumes the pre-serialized OpenAI-shaped
/// arrays; `/v1/messages` needs the original messages so it can serialize them
/// into Anthropic's wire shape instead.
struct RawTurn {
    system: String,
    messages: Vec<ChatMessage>,
    tools: Vec<ToolDefinition>,
}

impl RawTurn {
    /// Whether this turn carries image input, including images nested inside a
    /// tool result (a screenshot returned by a tool is the common case).
    fn has_images(&self) -> bool {
        self.messages.iter().any(|message| {
            message.content.iter().any(|block| match block {
                ContentBlock::Image { .. } => true,
                ContentBlock::ToolResult { content, .. } => content.contains("data:image/"),
                _ => false,
            })
        })
    }
}

#[async_trait]
impl Provider for CopilotApiProvider {
    async fn complete(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        self.wait_for_init().await;

        self.get_bearer_token().await.map_err(|e| {
            jcode_base::logging::warn(&format!(
                "Copilot bearer token acquisition failed (will trigger fallback): {}",
                e
            ));
            e
        })?;

        let is_user_initiated = self.is_user_initiated(messages);
        if is_user_initiated {
            self.user_turn_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        let built_messages = Self::build_messages(system, messages);
        let built_tools = Self::build_tools(tools);
        let model_for_fingerprint = self.model();
        let mut canonical_payload = json!({
            "model": &model_for_fingerprint,
            "messages": &built_messages,
            "tools": &built_tools,
        });
        Self::add_max_token_parameter(&mut canonical_payload, &model_for_fingerprint, 32_768u32);
        self.add_reasoning_effort_parameter(&mut canonical_payload, &model_for_fingerprint);
        let system_value = built_messages
            .first()
            .filter(|message| message.get("role").and_then(|role| role.as_str()) == Some("system"))
            .cloned();
        let tools_value = if built_tools.is_empty() {
            None
        } else {
            Some(Value::Array(built_tools.clone()))
        };
        jcode_provider_core::fingerprint::log_provider_canonical_input(
            "copilot",
            &model_for_fingerprint,
            "chat_completions",
            &canonical_payload,
            &built_messages,
            system_value.as_ref(),
            tools_value.as_ref(),
            Some(built_tools.len()),
            &[("user_initiated", is_user_initiated.to_string())],
        );

        let raw = RawTurn {
            system: system.to_string(),
            messages: messages.to_vec(),
            tools: tools.to_vec(),
        };

        let (tx, rx) = mpsc::channel::<Result<StreamEvent>>(100);

        let provider = CopilotApiProvider {
            client: self.client.clone(),
            model: self.model.clone(),
            github_token: self.github_token.clone(),
            fetched_models: self.fetched_models.clone(),
            model_specs: self.model_specs.clone(),
            catalog_source: self.catalog_source.clone(),
            account_type: self.account_type.clone(),
            session_id: self.session_id.clone(),
            machine_id: self.machine_id.clone(),
            init_ready: self.init_ready.clone(),
            init_done: self.init_done.clone(),
            premium_mode: self.premium_mode.clone(),
            user_turn_count: self.user_turn_count.clone(),
            reasoning_effort: self.reasoning_effort.clone(),
            model_explicitly_selected: self.model_explicitly_selected.clone(),
            created_at: self.created_at,
        };

        tokio::spawn(async move {
            provider
                .stream_request(built_messages, built_tools, raw, is_user_initiated, tx)
                .await;
        });

        Ok(Box::pin(ReceiverStream::new(rx)))
    }

    fn name(&self) -> &str {
        "copilot"
    }

    fn model(&self) -> String {
        self.model
            .try_read()
            .map(|m| m.clone())
            .unwrap_or_else(|_| DEFAULT_MODEL.to_string())
    }

    fn set_model(&self, model: &str) -> Result<()> {
        // See `strip_own_model_prefix`: `--provider copilot` routes through this
        // runtime directly, so session restore hands it `copilot:<model>`.
        let trimmed = jcode_provider_core::strip_own_model_prefix(model, "copilot:");
        if trimmed.is_empty() {
            anyhow::bail!("Copilot model cannot be empty");
        }
        if trimmed.contains("[1m]") {
            anyhow::bail!(
                "1M context window models are not supported via Copilot. Use the Anthropic API directly."
            );
        }
        if let Ok(mut current) = self.model.try_write() {
            *current = trimmed.to_string();
            // Tier detection may still be in flight; record that this choice is
            // deliberate so it does not get replaced by the catalog default.
            self.model_explicitly_selected
                .store(true, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "Cannot change model while a request is in progress"
            ))
        }
    }

    /// No static model list: see [`Self::available_models_display`]. The
    /// reachable set is per-account, so there is nothing truthful to return
    /// before the live catalog arrives.
    fn available_models(&self) -> Vec<&'static str> {
        Vec::new()
    }

    /// The models this account can actually reach.
    ///
    /// Deliberately empty until the live `/models` catalog (or the cache of a
    /// previous one) has landed. There is no honest static answer: the reachable
    /// set is decided by the OAuth app the token was minted under, so a
    /// hardcoded list is wrong in both directions — it offers models the account
    /// cannot reach, which fail with HTTP 400
    /// `model_not_available_for_integrator` the moment they are picked, and it
    /// hides models the account can. Callers render the empty case as a
    /// "catalog still loading" placeholder, which is the truth.
    fn available_models_display(&self) -> Vec<String> {
        self.fetched_models
            .read()
            .map(|models| models.clone())
            .unwrap_or_default()
    }

    fn available_models_for_switching(&self) -> Vec<String> {
        self.available_models_display()
    }

    /// Refresh the catalog, unless one is already loaded and still young.
    ///
    /// The grace window exists so a burst of startup callers does not each fire
    /// a `/models` request. It must never suppress the *first* fetch: without a
    /// catalog there is nothing to serve the picker, and nothing else retries.
    async fn prefetch_models(&self) -> Result<()> {
        let grace_ms = Self::startup_prefetch_grace_ms();
        if self.has_catalog() && self.created_at.elapsed().as_millis() < u128::from(grace_ms) {
            jcode_base::logging::info(&format!(
                "Skipping Copilot model prefetch during startup grace window ({}ms); catalog already loaded",
                grace_ms
            ));
            return Ok(());
        }
        self.detect_tier_and_set_default().await;
        Ok(())
    }

    fn supports_compaction(&self) -> bool {
        true
    }

    fn model_catalog_detail(&self) -> String {
        self.model_catalog_detail_impl()
    }

    fn set_premium_mode(&self, mode: PremiumMode) {
        CopilotApiProvider::set_premium_mode(self, mode);
    }

    fn premium_mode(&self) -> PremiumMode {
        CopilotApiProvider::get_premium_mode(self)
    }

    /// Context window for the active model.
    ///
    /// The live catalog wins over the built-in table, which is both stale and
    /// wrong in each direction: it understates `claude-sonnet-4.6` as 128k when
    /// the account really gets 1M (forcing needless compaction) and overstates
    /// legacy models like `gpt-4` (risking overflow). The table remains the
    /// fallback for the window before the first `/models` response lands.
    fn context_window(&self) -> usize {
        let model = self.model();
        if let Ok(specs) = self.model_specs.read()
            && let Some(window) = specs.context_window_for(&model)
        {
            return window;
        }
        jcode_provider_core::context_limit_for_model_with_provider(&model, Some(self.name()))
            .unwrap_or(128_000)
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(CopilotApiProvider {
            client: self.client.clone(),
            model: Arc::new(RwLock::new(self.model())),
            github_token: self.github_token.clone(),
            fetched_models: self.fetched_models.clone(),
            model_specs: self.model_specs.clone(),
            catalog_source: self.catalog_source.clone(),
            account_type: self.account_type.clone(),
            session_id: self.session_id.clone(),
            machine_id: self.machine_id.clone(),
            init_ready: self.init_ready.clone(),
            init_done: self.init_done.clone(),
            premium_mode: self.premium_mode.clone(),
            user_turn_count: self.user_turn_count.clone(),
            reasoning_effort: self.reasoning_effort.clone(),
            // The fork owns its model slot, so it owns its selection state too:
            // sharing the flag would let a fork's model change unpin the parent.
            model_explicitly_selected: Arc::new(std::sync::atomic::AtomicBool::new(
                self.model_explicitly_selected
                    .load(std::sync::atomic::Ordering::Relaxed),
            )),
            created_at: self.created_at,
        })
    }

    fn reasoning_effort(&self) -> Option<String> {
        let model = self.model();
        if !self.model_supports_reasoning_effort(&model) {
            return None;
        }
        self.current_reasoning_effort()
    }

    fn set_reasoning_effort(&self, effort: &str) -> Result<()> {
        let model = self.model();
        let supported = self.efforts_for(&model);
        if supported.is_empty() {
            anyhow::bail!(
                "Copilot model '{}' does not accept a reasoning effort",
                model
            );
        }
        let normalized = effort.trim().to_lowercase();
        let Some(accepted) = supported
            .iter()
            .find(|value| value.eq_ignore_ascii_case(&normalized))
        else {
            anyhow::bail!(
                "Unsupported reasoning effort '{}' for Copilot model '{}'. Supported: {}",
                effort,
                model,
                supported.join(", ")
            );
        };
        let accepted = accepted.clone();
        let mut guard = self
            .reasoning_effort
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = Some(accepted);
        Ok(())
    }

    fn available_efforts(&self) -> Vec<&'static str> {
        self.efforts_for(&self.model())
            .iter()
            .filter_map(|effort| intern_effort(effort))
            .collect()
    }
}

#[cfg(test)]
#[path = "copilot_tests.rs"]
mod tests;
