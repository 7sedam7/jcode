//! Proves the enterprise deployment actually reaches the request path.
//!
//! Unit tests on `CopilotDeployment` only show the URLs are *built* correctly.
//! These drive the real functions and read the host back out of the resulting
//! connection error, which is the only way to see where a request was sent
//! without a live enterprise tenant.
//!
//! `.invalid` is reserved by RFC 2606 and never resolves, so the requests fail
//! at DNS with the target URL intact.

use jcode_base::auth::copilot_enterprise::{COPILOT_ENTERPRISE_URL_ENV, CopilotDeployment};

const TEST_DOMAIN: &str = "tenant.jcode-test.invalid";

/// Serializes these tests: they mutate a process-global environment variable.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Sets the enterprise override for the duration of the guard's life.
struct EnterpriseEnv {
    _guard: std::sync::MutexGuard<'static, ()>,
    previous: Option<String>,
}

impl EnterpriseEnv {
    fn set() -> Self {
        let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var(COPILOT_ENTERPRISE_URL_ENV).ok();
        jcode_base::env::set_var(COPILOT_ENTERPRISE_URL_ENV, TEST_DOMAIN);
        Self {
            _guard: guard,
            previous,
        }
    }
}

impl Drop for EnterpriseEnv {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => jcode_base::env::set_var(COPILOT_ENTERPRISE_URL_ENV, value),
            None => jcode_base::env::remove_var(COPILOT_ENTERPRISE_URL_ENV),
        }
    }
}

/// The full error chain, which carries the URL reqwest tried to reach.
fn error_chain(e: anyhow::Error) -> String {
    format!("{e:#}")
}

#[tokio::test]
async fn the_catalog_fetch_targets_the_enterprise_copilot_host() {
    let _env = EnterpriseEnv::set();
    let client = reqwest::Client::new();
    let chain = error_chain(
        jcode_base::auth::copilot::fetch_available_models(&client, "gho_test")
            .await
            .expect_err("an .invalid domain cannot resolve"),
    );

    assert!(
        chain.contains(&format!("copilot-api.{TEST_DOMAIN}/models")),
        "catalog fetch must go to the enterprise Copilot host, got: {chain}"
    );
    assert!(
        !chain.contains("api.githubcopilot.com"),
        "catalog fetch must not fall back to dotcom, got: {chain}"
    );
}

#[tokio::test]
async fn token_verification_targets_the_enterprise_copilot_host() {
    let _env = EnterpriseEnv::set();
    let client = reqwest::Client::new();
    let chain = error_chain(
        jcode_base::auth::copilot::verify_copilot_token(&client, "gho_test")
            .await
            .expect_err("an .invalid domain cannot resolve"),
    );

    assert!(
        chain.contains(&format!("copilot-api.{TEST_DOMAIN}/models")),
        "token check must go to the enterprise Copilot host, got: {chain}"
    );
}

#[tokio::test]
async fn the_device_flow_targets_the_enterprise_github_host() {
    let _env = EnterpriseEnv::set();
    let client = reqwest::Client::new();
    let chain = error_chain(
        jcode_base::auth::copilot::initiate_device_flow(&client)
            .await
            .expect_err("an .invalid domain cannot resolve"),
    );

    assert!(
        chain.contains(&format!("{TEST_DOMAIN}/login/device/code")),
        "device flow must go to the enterprise GitHub host, got: {chain}"
    );
    assert!(
        !chain.contains("github.com"),
        "device flow must not fall back to github.com, got: {chain}"
    );
}

#[tokio::test]
async fn the_username_lookup_targets_the_enterprise_rest_api() {
    let _env = EnterpriseEnv::set();
    let client = reqwest::Client::new();
    let chain = error_chain(
        jcode_base::auth::copilot::fetch_github_username(&client, "gho_test")
            .await
            .expect_err("an .invalid domain cannot resolve"),
    );

    assert!(
        chain.contains(&format!("api.{TEST_DOMAIN}/user")),
        "username lookup must go to the enterprise REST API, got: {chain}"
    );
}

#[tokio::test]
async fn without_the_override_everything_still_targets_dotcom() {
    let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let previous = std::env::var(COPILOT_ENTERPRISE_URL_ENV).ok();
    jcode_base::env::remove_var(COPILOT_ENTERPRISE_URL_ENV);

    let deployment = jcode_base::auth::copilot_enterprise::current_deployment();

    if let Some(value) = previous {
        jcode_base::env::set_var(COPILOT_ENTERPRISE_URL_ENV, value);
    }
    drop(guard);

    // A machine that has never configured enterprise must be unaffected.
    assert_eq!(deployment, CopilotDeployment::DotCom);
    assert_eq!(deployment.api_base(), "https://api.githubcopilot.com");
}
