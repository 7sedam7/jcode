//! GitHub Enterprise deployments of Copilot.
//!
//! An enterprise account authenticates against its own GitHub host and talks to
//! its own Copilot endpoint, so every URL jcode builds has to be derived from
//! the deployment rather than hardcoded to github.com. The mapping mirrors
//! OpenCode's:
//!
//! | | dotcom | enterprise (`<domain>`) |
//! |---|---|---|
//! | device code | `github.com/login/device/code` | `<domain>/login/device/code` |
//! | access token | `github.com/login/oauth/access_token` | `<domain>/login/oauth/access_token` |
//! | Copilot API | `api.githubcopilot.com` | `copilot-api.<domain>` |
//! | REST API | `api.github.com` | `api.<domain>` |
//!
//! The deployment is chosen at login and persisted next to the token, because a
//! token minted by an enterprise host is meaningless to dotcom and vice versa.
//!
//! Separately from the *host*, a seat has a *plan*. An enterprise plan on
//! github.com is common (a personal account granted a seat by a company org)
//! and needs no domain: GitHub reports the endpoint to use at
//! `/copilot_internal/user`, which is authoritative and is preferred over every
//! constructed URL above. See [`fetch_user_info`].

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, RwLock};

/// Environment override for the enterprise domain. Empty or unset means dotcom.
pub const COPILOT_ENTERPRISE_URL_ENV: &str = "GITHUB_COPILOT_ENTERPRISE_URL";

/// Which GitHub host serves this Copilot account.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum CopilotDeployment {
    /// github.com.
    #[default]
    DotCom,
    /// A GitHub Enterprise host, stored normalized (no scheme, no trailing
    /// slash, lowercase), e.g. `company.ghe.com`.
    Enterprise(String),
}

impl CopilotDeployment {
    /// Build a deployment from user input, returning [`CopilotDeployment::DotCom`]
    /// for anything that names github.com.
    pub fn from_domain_input(input: &str) -> Result<Self> {
        let domain = normalize_domain(input)?;
        if domain == "github.com" || domain == "api.github.com" {
            return Ok(Self::DotCom);
        }
        Ok(Self::Enterprise(domain))
    }

    /// The enterprise domain, or `None` on dotcom.
    pub fn enterprise_domain(&self) -> Option<&str> {
        match self {
            Self::DotCom => None,
            Self::Enterprise(domain) => Some(domain),
        }
    }

    pub fn is_enterprise(&self) -> bool {
        matches!(self, Self::Enterprise(_))
    }

    /// Host that owns the OAuth credential. Doubles as the `hosts.json` key, so
    /// a dotcom token and an enterprise token can coexist in one file.
    pub fn host(&self) -> &str {
        match self {
            Self::DotCom => "github.com",
            Self::Enterprise(domain) => domain,
        }
    }

    pub fn device_code_url(&self) -> String {
        format!("https://{}/login/device/code", self.host())
    }

    pub fn access_token_url(&self) -> String {
        format!("https://{}/login/oauth/access_token", self.host())
    }

    /// Base URL of the Copilot API. Enterprise deployments front it at
    /// `copilot-api.<domain>`; the dotcom endpoint rejects their tokens.
    pub fn api_base(&self) -> String {
        match self {
            Self::DotCom => super::copilot::COPILOT_API_BASE.to_string(),
            Self::Enterprise(domain) => format!("https://copilot-api.{domain}"),
        }
    }

    /// GitHub REST API base, used to resolve the account's username.
    pub fn rest_api_base(&self) -> String {
        match self {
            Self::DotCom => "https://api.github.com".to_string(),
            Self::Enterprise(domain) => format!("https://api.{domain}"),
        }
    }
}

impl std::fmt::Display for CopilotDeployment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DotCom => write!(f, "github.com"),
            Self::Enterprise(domain) => write!(f, "{domain} (enterprise)"),
        }
    }
}

/// Strip scheme, path and trailing slash from user-supplied domain input.
///
/// Accepts what OpenCode's prompt accepts: `company.ghe.com`,
/// `https://company.ghe.com`, `https://company.ghe.com/`.
pub fn normalize_domain(input: &str) -> Result<String> {
    let trimmed = input.trim();
    anyhow::ensure!(!trimmed.is_empty(), "Enterprise domain must not be empty");

    let without_scheme = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .unwrap_or(trimmed);
    // Drop any path, query or fragment the user pasted along with the host.
    let host = without_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .trim()
        .trim_end_matches('.')
        .to_ascii_lowercase();

    anyhow::ensure!(
        !host.is_empty() && host.contains('.') && !host.contains(' ') && !host.contains('@'),
        "'{input}' is not a valid domain (expected something like 'company.ghe.com')"
    );
    // A port would produce an invalid `copilot-api.<domain>` hostname.
    anyhow::ensure!(
        !host.contains(':'),
        "Enterprise domain must not include a port or scheme: '{input}'"
    );
    Ok(host)
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedDeployment {
    /// Absent or empty means dotcom.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    enterprise_domain: Option<String>,
}

fn deployment_path() -> PathBuf {
    super::copilot::copilot_config_dir().join("deployment.json")
}

/// The deployment jcode should use: the environment override if set, otherwise
/// whatever the last successful login persisted, otherwise dotcom.
pub fn current_deployment() -> CopilotDeployment {
    if let Ok(raw) = std::env::var(COPILOT_ENTERPRISE_URL_ENV) {
        let raw = raw.trim();
        if !raw.is_empty() {
            match CopilotDeployment::from_domain_input(raw) {
                Ok(deployment) => return deployment,
                Err(e) => {
                    crate::logging::warn(&format!("Ignoring {COPILOT_ENTERPRISE_URL_ENV}: {e}"))
                }
            }
        } else {
            // An explicitly blank override means "use dotcom", which must beat
            // a persisted enterprise domain.
            return CopilotDeployment::DotCom;
        }
    }
    load_persisted_deployment().unwrap_or_default()
}

fn load_persisted_deployment() -> Option<CopilotDeployment> {
    let raw = std::fs::read_to_string(deployment_path()).ok()?;
    let persisted: PersistedDeployment = serde_json::from_str(&raw).ok()?;
    let domain = persisted.enterprise_domain?;
    CopilotDeployment::from_domain_input(&domain).ok()
}

/// Record the deployment a successful login used, so later sessions build the
/// same URLs without the user re-entering the domain.
pub fn save_deployment(deployment: &CopilotDeployment) -> Result<()> {
    let path = deployment_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
        crate::platform::set_directory_permissions_owner_only(parent)
            .with_context(|| format!("Failed to secure {}", parent.display()))?;
    }
    let payload = PersistedDeployment {
        enterprise_domain: deployment.enterprise_domain().map(str::to_string),
    };
    let json = serde_json::to_string_pretty(&payload)?;
    crate::storage::write_text_secret(&path, &json)
        .with_context(|| format!("Failed to write {}", path.display()))
}

#[cfg(test)]
#[path = "copilot_enterprise_tests.rs"]
mod tests;

/// What `GET /copilot_internal/user` reports about the authenticated seat.
///
/// This is how GitHub itself tells a client where to send Copilot traffic and
/// what the account is entitled to. Guessing either from the plan name is
/// wrong: an enterprise seat on github.com is served from
/// `api.enterprise.githubcopilot.com`, which no amount of string-building from
/// "github.com" produces.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct CopilotUserInfo {
    #[serde(default)]
    pub login: String,
    /// `individual`, `business`, `enterprise`, `free`, ...
    #[serde(default)]
    pub copilot_plan: String,
    #[serde(default)]
    pub access_type_sku: String,
    #[serde(default)]
    pub chat_enabled: bool,
    #[serde(default)]
    pub organization_login_list: Vec<String>,
    #[serde(default)]
    pub endpoints: Option<CopilotEndpoints>,
}

/// Service URLs GitHub assigns to this seat.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct CopilotEndpoints {
    /// Copilot API base. Authoritative — prefer it over any constructed URL.
    #[serde(default)]
    pub api: Option<String>,
}

impl CopilotUserInfo {
    /// The Copilot API base GitHub assigned to this seat, if it gave one.
    pub fn api_base(&self) -> Option<&str> {
        self.endpoints
            .as_ref()?
            .api
            .as_deref()
            .map(str::trim)
            .filter(|base| base.starts_with("https://"))
            .map(|base| base.trim_end_matches('/'))
    }

    /// Plan mapped onto jcode's account-type enum.
    pub fn account_type(&self) -> super::copilot::CopilotAccountType {
        use super::copilot::CopilotAccountType;
        match self.copilot_plan.trim().to_ascii_lowercase().as_str() {
            "enterprise" => CopilotAccountType::Enterprise,
            "business" => CopilotAccountType::Business,
            "individual" | "free" | "pro" | "pro_plus" | "individual_pro" => {
                CopilotAccountType::Individual
            }
            _ => CopilotAccountType::Unknown,
        }
    }
}

/// API base discovered from `/copilot_internal/user`, cached for the token that
/// produced it.
///
/// A process can briefly hold old and new Copilot providers while a login is
/// propagated to the persistent server. Keying the endpoint by a one-way token
/// fingerprint prevents either provider from borrowing the other account's
/// routing decision without retaining another plaintext copy of the token.
static DISCOVERED_API_BASES: LazyLock<RwLock<HashMap<[u8; 32], String>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));
static ENDPOINT_GENERATION: AtomicU64 = AtomicU64::new(1);

fn token_fingerprint(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

/// Stable, non-reversible key for account-scoped caches.
pub fn token_cache_key(token: &str) -> String {
    hex::encode(token_fingerprint(token))
}

/// Ask GitHub where to send this token's Copilot traffic, and what it may use.
///
/// Caches the discovered API base as a side effect.
pub async fn fetch_user_info(client: &reqwest::Client, token: &str) -> Result<CopilotUserInfo> {
    let generation = ENDPOINT_GENERATION.load(Ordering::Acquire);
    let deployment = current_deployment();
    let url = format!("{}/copilot_internal/user", deployment.rest_api_base());
    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {token}"))
        .header("User-Agent", super::copilot::EDITOR_VERSION)
        .header("Accept", "application/json")
        .send()
        .await
        .with_context(|| format!("Failed to reach {url}"))?;

    let status = resp.status();
    if !status.is_success() {
        let body = crate::util::http_error_body(resp, "HTTP error").await;
        anyhow::bail!("Copilot seat lookup failed (HTTP {status}): {body}");
    }

    let info: CopilotUserInfo = resp
        .json()
        .await
        .context("Failed to parse the Copilot seat response")?;

    let base = info
        .api_base()
        .map(str::to_string)
        .unwrap_or_else(|| deployment.api_base());
    record_discovered_api_base_if_current(token, &base, generation);
    Ok(info)
}

/// Resolve and cache the inference endpoint assigned to this token's seat.
///
/// `/models` succeeds on the public host even when a github.com account has an
/// Enterprise seat whose inference calls must use
/// `api.enterprise.githubcopilot.com`. Every request path therefore calls this
/// before it captures a base URL instead of treating catalog success as proof
/// that the default endpoint is valid.
pub async fn ensure_api_base(client: &reqwest::Client, token: &str) -> Result<String> {
    for _ in 0..3 {
        if let Some(base) = discovered_api_base_for(token) {
            return Ok(base);
        }
        fetch_user_info(client, token).await?;
    }
    anyhow::bail!("Copilot credentials changed repeatedly during endpoint discovery")
}

/// Publish an API base discovered from GitHub for one credential.
pub fn record_discovered_api_base_for(token: &str, base: &str) {
    let base = base.trim().trim_end_matches('/');
    if base.is_empty() {
        return;
    }
    let token_fingerprint = token_fingerprint(token);
    if let Ok(mut cached) = DISCOVERED_API_BASES.write() {
        if cached
            .get(&token_fingerprint)
            .is_none_or(|cached| cached != base)
        {
            crate::logging::info(&format!("Copilot API endpoint discovered: {base}"));
        }
        cached.insert(token_fingerprint, base.to_string());
    }
}

fn record_discovered_api_base_if_current(token: &str, base: &str, generation: u64) {
    let base = base.trim().trim_end_matches('/');
    if base.is_empty() {
        return;
    }
    let fingerprint = token_fingerprint(token);
    if let Ok(mut cached) = DISCOVERED_API_BASES.write()
        && ENDPOINT_GENERATION.load(Ordering::Acquire) == generation
    {
        cached.insert(fingerprint, base.to_string());
    }
}

/// Return the cached API base only when it belongs to `token`.
pub fn discovered_api_base_for(token: &str) -> Option<String> {
    let fingerprint = token_fingerprint(token);
    DISCOVERED_API_BASES
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&fingerprint)
        .cloned()
}

/// Forget the endpoint for `token`, leaving another credential's entry intact.
pub fn clear_discovered_api_base_for(token: &str) {
    ENDPOINT_GENERATION.fetch_add(1, Ordering::AcqRel);
    let fingerprint = token_fingerprint(token);
    if let Ok(mut cached) = DISCOVERED_API_BASES.write() {
        cached.remove(&fingerprint);
    }
}

/// Forget every discovered endpoint. Used by isolated tests.
pub fn clear_discovered_api_base() {
    ENDPOINT_GENERATION.fetch_add(1, Ordering::AcqRel);
    if let Ok(mut cached) = DISCOVERED_API_BASES.write() {
        cached.clear();
    }
}

/// Base URL for a specific Copilot credential.
///
/// The endpoint GitHub assigned this token wins over the configured
/// deployment's default. Call [`ensure_api_base`] before inference so the
/// fallback is used only when seat discovery genuinely failed.
pub fn api_base_for(token: &str) -> String {
    discovered_api_base_for(token).unwrap_or_else(|| current_deployment().api_base())
}
