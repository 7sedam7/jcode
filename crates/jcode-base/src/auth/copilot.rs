use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{LazyLock, RwLock};

/// Cached GitHub token resolved from file or subprocess sources, with the
/// time it was cached. Env vars are intentionally NOT served from this cache:
/// they are cheap to read and must take effect immediately when they change.
/// The TTL bounds how long a deleted/changed credential file keeps working.
static GITHUB_TOKEN_CACHE: LazyLock<RwLock<Option<(String, std::time::Instant)>>> =
    LazyLock::new(|| RwLock::new(None));
const GITHUB_TOKEN_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(300);
const FAILED_VALIDATION_AUTO_USE_TTL_MS: i64 = 24 * 60 * 60 * 1000;

fn cached_github_token() -> Option<String> {
    GITHUB_TOKEN_CACHE.read().ok().and_then(|value| {
        value.as_ref().and_then(|(token, cached_at)| {
            (cached_at.elapsed() < GITHUB_TOKEN_CACHE_TTL).then(|| token.clone())
        })
    })
}

fn cache_github_token(token: &str) {
    if let Ok(mut cache) = GITHUB_TOKEN_CACHE.write() {
        *cache = Some((token.to_string(), std::time::Instant::now()));
    }
}

pub fn invalidate_github_token_cache() {
    if let Ok(mut cache) = GITHUB_TOKEN_CACHE.write() {
        *cache = None;
    }
}

/// Default OAuth client ID for the GitHub Copilot device flow.
///
/// Override with the `GITHUB_CLIENT_ID` env var. The device flow uses no client
/// secret, so a client ID is safe to embed and safe to set from the environment.
///
/// **This choice decides which models the account can reach.** GitHub derives a
/// request's "integrator" identity from the OAuth app that minted the token, not
/// from the `Copilot-Integration-Id` header, and each integrator gets its own
/// model catalog. This default is the copilot-language-server app, whose catalog
/// omits several current models. A token already minted under one app keeps that
/// app's catalog, so changing this var only takes effect after re-running
/// `jcode login --provider copilot`.
///
/// TODO: replace this with jcode's own registered OAuth App. It is currently VS
/// Code's client ID, which is why the consent screen reads "Visual Studio Code".
pub const GITHUB_COPILOT_CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";

/// Env var overriding the Copilot device-flow client ID.
pub const GITHUB_COPILOT_CLIENT_ID_ENV: &str = "GITHUB_CLIENT_ID";

/// The OAuth client ID to use for the Copilot device flow.
///
/// Copilot API access is granted on the user's token, so a non-empty override is
/// honored as-is. An unset or blank env var falls back to the default.
pub fn github_copilot_client_id() -> String {
    std::env::var(GITHUB_COPILOT_CLIENT_ID_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| GITHUB_COPILOT_CLIENT_ID.to_string())
}

/// GitHub endpoints for Copilot auth
pub const GITHUB_DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
pub const GITHUB_ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
pub const COPILOT_TOKEN_URL: &str = "https://api.github.com/copilot_internal/v2/token";

/// Copilot API base URL
pub const COPILOT_API_BASE: &str = "https://api.githubcopilot.com";

/// API version sent on Copilot requests made from the auth layer.
pub const COPILOT_AUTH_API_VERSION: &str = "2026-06-01";
pub const COPILOT_CONFIG_JSON_SOURCE_ID: &str = "copilot_config_json";
pub const COPILOT_HOSTS_AUTH_SOURCE_ID: &str = "copilot_hosts_json";
pub const COPILOT_APPS_AUTH_SOURCE_ID: &str = "copilot_apps_json";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalCopilotAuthSource {
    ConfigJson,
    HostsJson,
    AppsJson,
    OpenCodeAuth,
    PiAuth,
}

impl ExternalCopilotAuthSource {
    pub fn source_id(self) -> &'static str {
        match self {
            Self::ConfigJson => COPILOT_CONFIG_JSON_SOURCE_ID,
            Self::HostsJson => COPILOT_HOSTS_AUTH_SOURCE_ID,
            Self::AppsJson => COPILOT_APPS_AUTH_SOURCE_ID,
            Self::OpenCodeAuth => crate::auth::external::OPENCODE_AUTH_JSON_SOURCE_ID,
            Self::PiAuth => crate::auth::external::PI_AUTH_JSON_SOURCE_ID,
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::ConfigJson => "GitHub Copilot CLI ~/.copilot/config.json",
            Self::HostsJson => "GitHub Copilot CLI hosts.json",
            Self::AppsJson => "GitHub Copilot apps.json",
            Self::OpenCodeAuth => "OpenCode auth.json",
            Self::PiAuth => "pi auth.json",
        }
    }

    pub fn path(self) -> PathBuf {
        match self {
            Self::ConfigJson => copilot_cli_dir().join("config.json"),
            Self::HostsJson => legacy_copilot_config_dir().join("hosts.json"),
            Self::AppsJson => legacy_copilot_config_dir().join("apps.json"),
            Self::OpenCodeAuth => crate::auth::external::ExternalAuthSource::OpenCode
                .path()
                .unwrap_or_default(),
            Self::PiAuth => crate::auth::external::ExternalAuthSource::Pi
                .path()
                .unwrap_or_default(),
        }
    }
}

/// Required headers for Copilot API requests
pub const EDITOR_VERSION: &str = "jcode/1.0";
pub const EDITOR_PLUGIN_VERSION: &str = "jcode/1.0";
pub const COPILOT_INTEGRATION_ID: &str = "vscode-chat";

/// Response from GitHub device code endpoint
#[derive(Debug, Deserialize)]
pub struct DeviceCodeResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    pub interval: u64,
}

/// Response from GitHub access token endpoint
#[derive(Debug, Deserialize)]
pub struct AccessTokenResponse {
    pub access_token: Option<String>,
    pub token_type: Option<String>,
    pub scope: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}

/// Response from Copilot token exchange endpoint
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CopilotTokenResponse {
    pub token: String,
    pub expires_at: i64,
}

/// Cached Copilot API token with expiry
#[derive(Debug, Clone)]
pub struct CopilotApiToken {
    pub token: String,
    pub expires_at: i64,
}

impl CopilotApiToken {
    pub fn is_expired(&self) -> bool {
        let now = chrono::Utc::now().timestamp();
        // Refresh 60 seconds before actual expiry
        now >= self.expires_at - 60
    }
}

/// Load a GitHub OAuth token from standard Copilot/CLI config locations.
///
/// Checks in order:
/// 1. COPILOT_GITHUB_TOKEN environment variable
/// 2. GH_TOKEN environment variable
/// 3. GITHUB_TOKEN environment variable
/// 4. ~/.copilot/config.json (official Copilot CLI plaintext fallback)
/// 5. ~/.config/github-copilot/hosts.json (legacy Copilot CLI)
/// 6. ~/.config/github-copilot/apps.json (legacy VS Code)
/// 7. trusted OpenCode/pi auth.json OAuth entries
/// 8. optional `gh auth token` fallback when JCODE_COPILOT_ALLOW_GH_AUTH_TOKEN=1
pub fn load_github_token() -> Result<String> {
    // Env vars first: cheap to read and they must win immediately when the
    // user changes them, so they are never served from (or shadowed by) the
    // file-source cache below.
    for env_key in ["COPILOT_GITHUB_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(token) = std::env::var(env_key)
            && !token.trim().is_empty()
        {
            return Ok(token.trim().to_string());
        }
    }

    if let Some(token) = cached_github_token() {
        return Ok(token);
    }

    let config_path = ExternalCopilotAuthSource::ConfigJson.path();
    if crate::config::Config::external_auth_source_allowed_for_path(
        COPILOT_CONFIG_JSON_SOURCE_ID,
        &config_path,
    ) && let Ok(token) = load_token_from_config_json(&config_path)
    {
        cache_github_token(&token);
        return Ok(token);
    }

    let hosts_path = ExternalCopilotAuthSource::HostsJson.path();
    if crate::config::Config::external_auth_source_allowed_for_path(
        COPILOT_HOSTS_AUTH_SOURCE_ID,
        &hosts_path,
    ) && let Ok(token) = load_token_from_json(&hosts_path)
    {
        cache_github_token(&token);
        return Ok(token);
    }

    let apps_path = ExternalCopilotAuthSource::AppsJson.path();
    if crate::config::Config::external_auth_source_allowed_for_path(
        COPILOT_APPS_AUTH_SOURCE_ID,
        &apps_path,
    ) && let Ok(token) = load_token_from_json(&apps_path)
    {
        cache_github_token(&token);
        return Ok(token);
    }

    if let Some(token) = crate::auth::external::load_copilot_oauth_token() {
        cache_github_token(&token);
        return Ok(token);
    }

    if allow_gh_cli_fallback()
        && let Some(token) = load_token_from_gh_cli()
    {
        cache_github_token(&token);
        return Ok(token);
    }

    anyhow::bail!(
        "GitHub Copilot token not found. \
         Set COPILOT_GITHUB_TOKEN/GH_TOKEN/GITHUB_TOKEN, run `jcode login --provider copilot`, \
         or set JCODE_COPILOT_ALLOW_GH_AUTH_TOKEN=1 to explicitly reuse `gh auth token`."
    )
}

fn allow_gh_cli_fallback() -> bool {
    std::env::var("JCODE_COPILOT_ALLOW_GH_AUTH_TOKEN")
        .ok()
        .map(|value| {
            let value = value.trim();
            !value.is_empty() && value != "0" && !value.eq_ignore_ascii_case("false")
        })
        .unwrap_or(false)
}

fn copilot_env_token_present() -> bool {
    ["COPILOT_GITHUB_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"]
        .into_iter()
        .any(|env_key| {
            std::env::var(env_key)
                .ok()
                .map(|token| !token.trim().is_empty())
                .unwrap_or(false)
        })
}

/// Return true when a recent `auth-test` proved the discovered Copilot token is
/// not exchangeable for a Copilot API token.
///
/// Copilot is unusual because a local GitHub OAuth token can exist while the
/// account is not entitled to Copilot, or the token is otherwise rejected by the
/// Copilot token service. Presence-only checks are still useful for explicit
/// diagnostics, but they must not cause startup/default-provider selection to
/// silently choose Copilot as a usable provider after a known token-exchange
/// failure. Environment tokens are treated as an explicit override because they
/// may be a newly supplied credential that is not represented by the saved
/// validation record.
pub fn validation_failure_blocks_auto_use() -> bool {
    if copilot_env_token_present() {
        return false;
    }

    let Some(record) = crate::auth::validation::get("copilot") else {
        return false;
    };
    if record.success {
        return false;
    }

    let age_ms = chrono::Utc::now()
        .timestamp_millis()
        .saturating_sub(record.checked_at_ms);
    if age_ms > FAILED_VALIDATION_AUTO_USE_TTL_MS {
        return false;
    }

    let summary = record.summary.to_ascii_lowercase();
    summary.contains("copilot token exchange failed")
        && (summary.contains("http 401")
            || summary.contains("http 403")
            || summary.contains("feature_flag_blocked")
            || summary.contains("resource not accessible"))
}

/// Check if Copilot credentials are available (without loading the full token)
pub fn has_copilot_credentials() -> bool {
    load_github_token().is_ok()
}

/// Fast local Copilot credential probe for startup-sensitive paths.
///
/// This intentionally avoids the `gh auth token` fallback because spawning the
/// GitHub CLI is too expensive for the fast auth snapshot.
pub fn has_copilot_credentials_fast() -> bool {
    use crate::auth::external::{ExternalAuthSource, source_has_copilot_oauth};

    for env_key in ["COPILOT_GITHUB_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(token) = std::env::var(env_key)
            && !token.trim().is_empty()
        {
            cache_github_token(token.trim());
            return true;
        }
    }

    let config_path = ExternalCopilotAuthSource::ConfigJson.path();
    if config_path.exists()
        && crate::config::Config::external_auth_source_allowed_for_path_cached(
            COPILOT_CONFIG_JSON_SOURCE_ID,
            &config_path,
        )
        && let Ok(token) = load_token_from_config_json(&config_path)
    {
        cache_github_token(&token);
        return true;
    }

    let hosts_path = ExternalCopilotAuthSource::HostsJson.path();
    if hosts_path.exists()
        && crate::config::Config::external_auth_source_allowed_for_path_cached(
            COPILOT_HOSTS_AUTH_SOURCE_ID,
            &hosts_path,
        )
        && let Ok(token) = load_token_from_json(&hosts_path)
    {
        cache_github_token(&token);
        return true;
    }

    let apps_path = ExternalCopilotAuthSource::AppsJson.path();
    if apps_path.exists()
        && crate::config::Config::external_auth_source_allowed_for_path_cached(
            COPILOT_APPS_AUTH_SOURCE_ID,
            &apps_path,
        )
        && let Ok(token) = load_token_from_json(&apps_path)
    {
        cache_github_token(&token);
        return true;
    }

    for source in [ExternalAuthSource::OpenCode, ExternalAuthSource::Pi] {
        let Ok(path) = source.path() else {
            continue;
        };
        if !path.exists() {
            continue;
        }
        if crate::config::Config::external_auth_source_allowed_for_path_cached(
            source.source_id(),
            &path,
        ) && source_has_copilot_oauth(source)
        {
            if let Some(token) = crate::auth::external::load_copilot_oauth_token() {
                cache_github_token(&token);
            }
            return true;
        }
    }

    false
}

pub fn preferred_external_auth_source() -> Option<ExternalCopilotAuthSource> {
    [
        ExternalCopilotAuthSource::ConfigJson,
        ExternalCopilotAuthSource::HostsJson,
        ExternalCopilotAuthSource::AppsJson,
        ExternalCopilotAuthSource::OpenCodeAuth,
        ExternalCopilotAuthSource::PiAuth,
    ]
    .into_iter()
    .find(|source| match source {
        ExternalCopilotAuthSource::OpenCodeAuth => {
            let path = source.path();
            path.exists()
                && !crate::auth::external::source_allowed(
                    crate::auth::external::ExternalAuthSource::OpenCode,
                )
                && crate::auth::external::source_has_copilot_oauth(
                    crate::auth::external::ExternalAuthSource::OpenCode,
                )
        }
        ExternalCopilotAuthSource::PiAuth => {
            let path = source.path();
            path.exists()
                && !crate::auth::external::source_allowed(
                    crate::auth::external::ExternalAuthSource::Pi,
                )
                && crate::auth::external::source_has_copilot_oauth(
                    crate::auth::external::ExternalAuthSource::Pi,
                )
        }
        _ => source.path().exists(),
    })
}

pub fn has_unconsented_external_auth() -> Option<ExternalCopilotAuthSource> {
    let source = preferred_external_auth_source()?;
    let allowed = match source {
        ExternalCopilotAuthSource::OpenCodeAuth => crate::auth::external::source_allowed(
            crate::auth::external::ExternalAuthSource::OpenCode,
        ),
        ExternalCopilotAuthSource::PiAuth => {
            crate::auth::external::source_allowed(crate::auth::external::ExternalAuthSource::Pi)
        }
        _ => crate::config::Config::external_auth_source_allowed_for_path(
            source.source_id(),
            &source.path(),
        ),
    };
    if allowed { None } else { Some(source) }
}

pub fn trust_external_auth_source(source: ExternalCopilotAuthSource) -> Result<()> {
    match source {
        ExternalCopilotAuthSource::OpenCodeAuth => {
            crate::auth::external::trust_external_auth_source(
                crate::auth::external::ExternalAuthSource::OpenCode,
            )?;
        }
        ExternalCopilotAuthSource::PiAuth => {
            crate::auth::external::trust_external_auth_source(
                crate::auth::external::ExternalAuthSource::Pi,
            )?;
        }
        _ => {
            crate::config::Config::allow_external_auth_source_for_path(
                source.source_id(),
                &source.path(),
            )?;
        }
    }
    super::AuthStatus::invalidate_cache();
    Ok(())
}

fn copilot_cli_dir() -> PathBuf {
    if let Ok(path) = std::env::var("JCODE_HOME") {
        return PathBuf::from(path).join("external").join(".copilot");
    }

    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".copilot")
}

/// Directory holding the Copilot credential and its deployment record.
pub(crate) fn copilot_config_dir() -> PathBuf {
    legacy_copilot_config_dir()
}

fn legacy_copilot_config_dir() -> PathBuf {
    if let Ok(path) = std::env::var("JCODE_HOME") {
        return PathBuf::from(path)
            .join("external")
            .join(".config")
            .join("github-copilot");
    }

    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(xdg).join("github-copilot")
    } else if cfg!(windows) {
        let local_app_data = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_default();
            format!("{}/AppData/Local", home)
        });
        PathBuf::from(local_app_data).join("github-copilot")
    } else {
        let home = std::env::var("HOME").unwrap_or_default();
        PathBuf::from(home).join(".config").join("github-copilot")
    }
}

pub fn saved_hosts_path() -> PathBuf {
    legacy_copilot_config_dir().join("hosts.json")
}

fn load_token_from_config_json(path: &Path) -> Result<String> {
    let path = crate::storage::validate_external_auth_file(path)?;
    let data = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    let value: Value = serde_json::from_str(&data)
        .with_context(|| format!("Failed to parse {}", path.display()))?;
    find_token_in_value(&value)
        .ok_or_else(|| anyhow::anyhow!("No GitHub token found in {}", path.display()))
}

fn find_token_in_value(value: &Value) -> Option<String> {
    match value {
        Value::String(token) => normalize_candidate_token(token),
        Value::Array(items) => items.iter().find_map(find_token_in_value),
        Value::Object(map) => {
            for key in ["oauth_token", "token", "github_token", "access_token"] {
                if let Some(token) = map.get(key).and_then(find_token_in_value) {
                    return Some(token);
                }
            }
            map.values().find_map(find_token_in_value)
        }
        _ => None,
    }
}

fn normalize_candidate_token(token: &str) -> Option<String> {
    let token = token.trim();
    if token.is_empty() {
        return None;
    }

    if token.starts_with("gho_")
        || token.starts_with("ghu_")
        || token.starts_with("github_pat_")
        || token.starts_with("ghs_")
    {
        return Some(token.to_string());
    }

    None
}

fn load_token_from_gh_cli() -> Option<String> {
    if !crate::auth::command_exists("gh") {
        return None;
    }

    let output = Command::new("gh").args(["auth", "token"]).output().ok()?;
    if !output.status.success() {
        return None;
    }

    let token = String::from_utf8(output.stdout).ok()?;
    normalize_candidate_token(&token)
}

/// Parse a Copilot config JSON file to extract the oauth_token.
/// Format: { "github.com": { "oauth_token": "gho_xxxx", "user": "..." } }
fn load_token_from_json(path: &Path) -> Result<String> {
    let path = crate::storage::validate_external_auth_file(path)?;
    let data = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read {}", path.display()))?;

    let config: HashMap<String, HashMap<String, serde_json::Value>> =
        serde_json::from_str(&data)
            .with_context(|| format!("Failed to parse {}", path.display()))?;

    let token = select_preferred_token(&config)
        .ok_or_else(|| anyhow::anyhow!("No oauth_token found in {}", path.display()))?;

    Ok(token.clone())
}

fn select_preferred_token(
    config: &HashMap<String, HashMap<String, serde_json::Value>>,
) -> Option<&String> {
    config
        .iter()
        .filter_map(|(host, value)| {
            let token = match value.get("oauth_token") {
                Some(serde_json::Value::String(token)) if !token.is_empty() => token,
                _ => return None,
            };

            let normalized_host = normalize_github_host_key(host)?;
            let raw_host = host.trim().to_ascii_lowercase();
            Some((
                github_host_priority(&raw_host, &normalized_host),
                normalized_host,
                raw_host,
                token,
            ))
        })
        .min_by(|left, right| {
            left.0
                .cmp(&right.0)
                .then_with(|| left.1.cmp(&right.1))
                .then_with(|| left.2.cmp(&right.2))
        })
        .map(|(_, _, _, token)| token)
}

fn github_host_priority(raw_host: &str, normalized_host: &str) -> u8 {
    // On an enterprise deployment its own credential must win, otherwise a
    // leftover dotcom token gets sent to the enterprise endpoint and 401s.
    if let Some(domain) = super::copilot_enterprise::current_deployment().enterprise_domain() {
        if raw_host == domain || normalized_host == domain {
            return 0;
        }
        return 1;
    }
    if raw_host == "github.com" {
        0
    } else if normalized_host == "github.com" {
        1
    } else if raw_host == "api.github.com" {
        2
    } else if normalized_host == "api.github.com" {
        3
    } else {
        4
    }
}

fn normalize_github_host_key(host: &str) -> Option<String> {
    let host = host.trim();
    if host.is_empty() {
        return None;
    }

    let host = host
        .strip_prefix("https://")
        .or_else(|| host.strip_prefix("http://"))
        .unwrap_or(host)
        .trim_end_matches('/');
    let host = host.split('/').next().unwrap_or_default().trim();
    // GitHub's `apps.json` keys carry a client-id suffix
    // (`github.com:Iv1.b507a08c87ecfe98`), so the host must also be split on
    // ':' or valid credentials are silently discarded (issue #641).
    // `split` always yields at least one element, so this cannot lose the host.
    let host = host.split(':').next().unwrap_or(host).trim();
    let host = host.to_ascii_lowercase();

    if host == "github.com" || host == "api.github.com" || host.ends_with(".github.com") {
        return Some(host);
    }
    // An enterprise deployment stores its credential under its own host, which
    // is not a github.com name; without this the token is silently discarded.
    match super::copilot_enterprise::current_deployment().enterprise_domain() {
        Some(domain) if host == domain || host == format!("api.{domain}") => Some(host),
        _ => None,
    }
}

/// Maximum attempts for the Copilot token exchange when GitHub returns a
/// transient 5xx (e.g. a 502 from a bad gateway).
const TOKEN_EXCHANGE_MAX_ATTEMPTS: u32 = 5;

/// Backoff delay (milliseconds) before retry attempt `attempt` (1-based count
/// of completed attempts). Exponential: 500ms, 1s, 2s, capped at 4s.
pub(crate) fn token_exchange_backoff_ms(attempt: u32) -> u64 {
    let base: u64 = 500;
    base.saturating_mul(1u64 << attempt.saturating_sub(1).min(6))
        .min(4_000)
}

/// Whether an HTTP status from the token exchange is worth retrying.
pub(crate) fn token_exchange_retryable_status(status: u16) -> bool {
    (500..600).contains(&status)
}

/// Validate a GitHub OAuth token against the Copilot API.
///
/// The token is sent directly as `Authorization: Bearer`; Copilot grants access
/// on the user's own token, so there is no exchange step and nothing to refresh.
/// `/models` is the cheapest authenticated endpoint, so it doubles as the
/// liveness probe.
///
/// Transient 5xx responses are retried with exponential backoff (issue #548);
/// 4xx responses fail immediately.
pub async fn verify_copilot_token(client: &reqwest::Client, github_token: &str) -> Result<()> {
    // Learn this seat's real endpoint before probing it. An enterprise seat on
    // github.com is served from api.enterprise.githubcopilot.com, and a probe
    // sent to the wrong base can fail for a reason that looks like a bad token.
    let _ = super::copilot_enterprise::fetch_user_info(client, github_token).await;
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        let resp = client
            .get(format!("{}/models", super::copilot_enterprise::api_base()))
            .header("Authorization", format!("Bearer {github_token}"))
            .header("User-Agent", EDITOR_VERSION)
            .header("X-GitHub-Api-Version", COPILOT_AUTH_API_VERSION)
            .send()
            .await
            .context("Failed to reach the Copilot API")?;

        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }

        let retryable = token_exchange_retryable_status(status.as_u16());
        if retryable && attempt < TOKEN_EXCHANGE_MAX_ATTEMPTS {
            let delay_ms = token_exchange_backoff_ms(attempt);
            crate::logging::warn(&format!(
                "Copilot auth check got transient HTTP {}, retrying in {}ms (attempt {}/{})",
                status, delay_ms, attempt, TOKEN_EXCHANGE_MAX_ATTEMPTS
            ));
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            continue;
        }

        let body = crate::util::http_error_body(resp, "HTTP error").await;
        if retryable {
            anyhow::bail!(
                "Copilot auth check failed after {} attempts (HTTP {}): {}",
                attempt,
                status,
                body
            );
        }
        anyhow::bail!("Copilot auth check failed (HTTP {}): {}", status, body);
    }
}

/// Run a live Copilot auth check and persist the result as a validation record.
///
/// This is the only definitive way to know a discovered GitHub token is actually
/// usable for Copilot: a token can exist locally while the account is banned,
/// not entitled, or otherwise rejected by the Copilot token service. We exchange
/// the GitHub OAuth token for a Copilot bearer token (the same call the live
/// provider makes) and record success/failure so presence-based readiness
/// surfaces (`validation_failure_blocks_auto_use`, `check_fast`) reflect reality.
///
/// Returns `Ok(())` when the token exchange succeeds, or the underlying error
/// (whose message embeds the HTTP status, e.g. `HTTP 401`/`HTTP 403`) otherwise.
pub async fn verify_copilot_credentials_live(client: &reqwest::Client) -> Result<()> {
    let github_token = load_github_token()?;
    let result = verify_copilot_token(client, &github_token).await;

    let summary = match &result {
        Ok(_) => "copilot auth check ok".to_string(),
        Err(err) => format!("{err}"),
    };
    let record = crate::auth::validation::ProviderValidationRecord {
        checked_at_ms: chrono::Utc::now().timestamp_millis(),
        success: result.is_ok(),
        provider_smoke_ok: Some(result.is_ok()),
        tool_smoke_ok: None,
        summary,
    };
    // Best-effort: a failure to persist must not change the live result.
    let _ = crate::auth::validation::save("copilot", record);
    // Refresh the auth snapshot so readiness surfaces pick up the new record.
    crate::auth::AuthStatus::invalidate_cache();

    result.map(|_| ())
}

/// Convenience wrapper around [`verify_copilot_credentials_live`] that builds a
/// short-lived HTTP client. Useful for callers (e.g. the TUI crate) that do not
/// depend on `reqwest` directly.
pub async fn verify_copilot_credentials_live_default() -> Result<()> {
    let client = reqwest::Client::new();
    verify_copilot_credentials_live(&client).await
}

/// Initiate GitHub OAuth device flow for Copilot authentication.
/// Returns the device code response with user instructions.
pub async fn initiate_device_flow(client: &reqwest::Client) -> Result<DeviceCodeResponse> {
    let client_id = github_copilot_client_id();
    let deployment = super::copilot_enterprise::current_deployment();
    let resp = client
        .post(deployment.device_code_url())
        .header("Accept", "application/json")
        .form(&[("client_id", client_id.as_str()), ("scope", "read:user")])
        .send()
        .await
        .context("Failed to initiate GitHub device flow")?;

    if !resp.status().is_success() {
        let body = crate::util::http_error_body(resp, "HTTP error").await;
        anyhow::bail!("GitHub device flow failed: {}", body);
    }

    resp.json::<DeviceCodeResponse>()
        .await
        .context("Failed to parse device code response")
}

/// Poll for the access token after user has authorized the device.
/// Returns the GitHub OAuth token (gho_xxx format).
pub async fn poll_for_access_token(
    client: &reqwest::Client,
    device_code: &str,
    interval: u64,
) -> Result<String> {
    let client_id = github_copilot_client_id();
    let deployment = super::copilot_enterprise::current_deployment();
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(interval)).await;

        let resp = client
            .post(deployment.access_token_url())
            .header("Accept", "application/json")
            .form(&[
                ("client_id", client_id.as_str()),
                ("device_code", device_code),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .send()
            .await
            .context("Failed to poll for access token")?;

        let token_resp: AccessTokenResponse = resp
            .json()
            .await
            .context("Failed to parse access token response")?;

        if let Some(token) = token_resp.access_token {
            return Ok(token);
        }

        match token_resp.error.as_deref() {
            Some("authorization_pending") => continue,
            Some("slow_down") => {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                continue;
            }
            Some("expired_token") => {
                anyhow::bail!("Device code expired. Please try again.");
            }
            Some("access_denied") => {
                anyhow::bail!("Authorization was denied by the user.");
            }
            Some(err) => {
                let desc = token_resp.error_description.unwrap_or_default();
                anyhow::bail!("GitHub auth error: {} - {}", err, desc);
            }
            None => {
                anyhow::bail!("Unexpected response from GitHub");
            }
        }
    }
}

/// Save a GitHub OAuth token to the standard Copilot config location.
pub fn save_github_token(token: &str, username: &str) -> Result<()> {
    let config_dir = legacy_copilot_config_dir();
    std::fs::create_dir_all(&config_dir)
        .with_context(|| format!("Failed to create {}", config_dir.display()))?;
    crate::platform::set_directory_permissions_owner_only(&config_dir)
        .with_context(|| format!("Failed to secure {}", config_dir.display()))?;

    let hosts_path = config_dir.join("hosts.json");

    let mut config: HashMap<String, HashMap<String, String>> =
        if let Ok(data) = std::fs::read_to_string(&hosts_path) {
            serde_json::from_str(&data).unwrap_or_default()
        } else {
            HashMap::new()
        };

    // Key by the deployment's host: an enterprise token is not valid on
    // dotcom, so overwriting a single "github.com" entry would silently
    // clobber one credential with the other.
    let deployment = super::copilot_enterprise::current_deployment();
    let mut entry = HashMap::new();
    entry.insert("user".to_string(), username.to_string());
    entry.insert("oauth_token".to_string(), token.to_string());
    config.insert(deployment.host().to_string(), entry);

    let json = serde_json::to_string_pretty(&config)?;
    crate::storage::write_text_secret(&hosts_path, &json)
        .with_context(|| format!("Failed to write {}", hosts_path.display()))?;

    // A token written by jcode's own device-login flow should be immediately
    // usable in future sessions. Without this, later reads treat the saved
    // hosts.json as an untrusted external auth source and appear to "lose"
    // the Copilot login after restart/new session.
    crate::config::Config::allow_external_auth_source_for_path(
        COPILOT_HOSTS_AUTH_SOURCE_ID,
        &hosts_path,
    )?;
    super::AuthStatus::invalidate_cache();

    Ok(())
}

/// Fetch the authenticated GitHub username using an OAuth token.
pub async fn fetch_github_username(client: &reqwest::Client, token: &str) -> Result<String> {
    let deployment = super::copilot_enterprise::current_deployment();
    let resp = client
        .get(format!("{}/user", deployment.rest_api_base()))
        .header("Authorization", format!("Bearer {}", token))
        .header("User-Agent", EDITOR_VERSION)
        .send()
        .await
        .context("Failed to fetch GitHub user")?;

    if !resp.status().is_success() {
        anyhow::bail!("Failed to fetch GitHub user (HTTP {})", resp.status());
    }

    #[derive(Deserialize)]
    struct GithubUser {
        login: String,
    }

    let user: GithubUser = resp.json().await.context("Failed to parse GitHub user")?;
    Ok(user.login)
}

/// The model catalog lives in its own module. It is re-exported here so the
/// `auth::copilot::` paths callers already use keep working.
#[path = "copilot_catalog.rs"]
mod catalog;
pub use catalog::*;

#[cfg(test)]
#[path = "copilot_auth_tests.rs"]
mod tests;
