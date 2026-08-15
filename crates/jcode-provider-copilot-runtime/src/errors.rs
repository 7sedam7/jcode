//! Turning Copilot's HTTP error bodies into messages a user can act on.
//!
//! Copilot returns a small number of failures whose raw body explains the
//! symptom but not the remedy. The worst offender is
//! `model_not_available_for_integrator`: the body lists models the account *can*
//! reach and blames the `Copilot-Integration-Id` header, but that header is not
//! what decides access. The integrator identity comes from the **OAuth app the
//! GitHub token was minted under**, which was verified directly: with a token
//! minted under one app, every value of `Copilot-Integration-Id` (including
//! `copilot-language-server`) returned HTTP 200 for a model the header-named
//! integrator cannot reach. Changing the header does nothing; only re-minting
//! the token under a different app does.

use jcode_base::auth::copilot::{GITHUB_COPILOT_CLIENT_ID_ENV, github_copilot_client_id};

/// Copilot's error code for a model outside the token's integrator catalog.
const MODEL_NOT_AVAILABLE_FOR_INTEGRATOR: &str = "model_not_available_for_integrator";

/// Copilot's error code for a model sent to an endpoint it does not serve.
///
/// Recoverable: the catalog says which endpoint a model speaks, so seeing this
/// means our copy of the catalog is stale rather than that the request is
/// impossible.
pub const UNSUPPORTED_API_FOR_MODEL: &str = "unsupported_api_for_model";

/// Extra guidance to append to a Copilot HTTP error, if we recognize it.
///
/// Returns `None` for errors whose own body is already actionable, so the
/// caller reports them unchanged.
pub fn guidance_for(status: u16, body: &str) -> Option<String> {
    // An enterprise deployment 401s every request when the token came from
    // github.com (or the reverse). The body says only "unauthorized", so name
    // the deployment: it is the setting that is almost always at fault.
    if status == 401 || status == 403 {
        let deployment = jcode_base::auth::copilot_enterprise::current_deployment();
        return Some(match deployment.enterprise_domain() {
            Some(domain) => format!(
                "\n\njcode is configured for the GitHub Enterprise deployment \
                 `{domain}` and sent this request to `{}`. A token minted on \
                 github.com is not valid there. Re-run \
                 `jcode login --provider copilot --enterprise {domain}`, or \
                 switch back with `jcode login --provider copilot --enterprise github.com`.",
                deployment.api_base()
            ),
            None => "\n\njcode used the github.com Copilot deployment. If this \
                     account is on GitHub Enterprise, log in against it with \
                     `jcode login --provider copilot --enterprise <your-domain>`."
                .to_string(),
        });
    }

    if status != 400 || !body.contains(MODEL_NOT_AVAILABLE_FOR_INTEGRATOR) {
        return None;
    }

    let client_id = github_copilot_client_id();
    Some(format!(
        "\n\nThis is not really about the requested model, and not about the \
         `Copilot-Integration-Id` header the message names. GitHub decides which \
         models a token may reach from the OAuth app that minted it. Your token \
         was minted under client ID `{client_id}`, whose catalog is the list \
         above.\n\nTo reach a model that is missing from that list, mint a token \
         under an app that offers it: set `{GITHUB_COPILOT_CLIENT_ID_ENV}` to \
         that app's client ID and re-run `jcode login --provider copilot`. \
         Re-authenticating alone is not enough — an existing token keeps the \
         catalog of the app it was minted under.\n\nOtherwise pick one of the \
         models listed above."
    ))
}

/// Append [`guidance_for`] to a Copilot error body when it applies.
pub fn annotate(status: u16, body: &str) -> String {
    match guidance_for(status, body) {
        Some(extra) => format!("{body}{extra}"),
        None => body.to_string(),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Verbatim body from a real failure, trimmed in the middle.
    const REAL_BODY: &str = r#"{"error":{"message":"The requested model is not available for integrator \"copilot-language-server\". Available models: [gpt-4.1 claude-opus-4.7 gpt-5.5]. Verify the correct Copilot-Integration-Id header is being sent.","code":"model_not_available_for_integrator","param":"model","type":"invalid_request_error"}}"#;

    #[test]
    fn recognizes_the_integrator_error() {
        let guidance = guidance_for(400, REAL_BODY).expect("should recognize this error");
        assert!(
            guidance.contains(GITHUB_COPILOT_CLIENT_ID_ENV),
            "guidance must name the env var that fixes it: {guidance}"
        );
        assert!(
            guidance.contains("jcode login --provider copilot"),
            "guidance must name the re-auth command: {guidance}"
        );
        assert!(
            guidance.contains(&github_copilot_client_id()),
            "guidance must name the client ID actually in effect: {guidance}"
        );
    }

    #[test]
    fn says_re_auth_alone_is_insufficient() {
        // The trap: users re-run login without changing the client ID, get the
        // same catalog, and conclude the fix did not work.
        let guidance = guidance_for(400, REAL_BODY).unwrap();
        assert!(guidance.contains("Re-authenticating alone is not enough"));
    }

    #[test]
    fn ignores_unrelated_errors() {
        assert_eq!(
            guidance_for(400, r#"{"error":{"code":"store_not_supported"}}"#),
            None
        );
        assert_eq!(guidance_for(429, REAL_BODY), None, "wrong status");
        assert_eq!(guidance_for(500, "internal error"), None);
    }

    /// The deployment override is process-global, so these tests must not run
    /// concurrently with each other.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Sets the deployment override for the life of the guard.
    pub(crate) struct DeploymentEnv {
        _guard: std::sync::MutexGuard<'static, ()>,
        previous: Option<String>,
    }

    impl DeploymentEnv {
        pub(crate) fn set(value: &str) -> Self {
            let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let env = jcode_base::auth::copilot_enterprise::COPILOT_ENTERPRISE_URL_ENV;
            let previous = std::env::var(env).ok();
            jcode_base::env::set_var(env, value);
            Self {
                _guard: guard,
                previous,
            }
        }
    }

    impl Drop for DeploymentEnv {
        fn drop(&mut self) {
            let env = jcode_base::auth::copilot_enterprise::COPILOT_ENTERPRISE_URL_ENV;
            match self.previous.take() {
                Some(value) => jcode_base::env::set_var(env, value),
                None => jcode_base::env::remove_var(env),
            }
        }
    }

    /// A 401 on an enterprise deployment nearly always means the token was
    /// minted on the wrong host, which the raw body never says.
    #[test]
    fn unauthorized_names_the_enterprise_deployment() {
        let _env = DeploymentEnv::set("company.ghe.com");
        let guidance = guidance_for(401, "unauthorized").expect("401 is actionable");
        assert!(guidance.contains("company.ghe.com"), "{guidance}");
        assert!(
            guidance.contains("copilot-api.company.ghe.com"),
            "must name the host actually contacted: {guidance}"
        );
    }

    #[test]
    fn unauthorized_on_dotcom_points_at_enterprise_as_the_likely_cause() {
        let _env = DeploymentEnv::set("");
        let guidance = guidance_for(403, "forbidden").expect("403 is actionable");
        assert!(guidance.contains("--enterprise"), "{guidance}");
    }

    #[test]
    fn annotate_preserves_the_original_body() {
        let annotated = annotate(400, REAL_BODY);
        assert!(
            annotated.starts_with(REAL_BODY),
            "must not lose the raw body"
        );
        assert!(
            annotated.len() > REAL_BODY.len(),
            "must have appended guidance"
        );

        let untouched = annotate(400, "something else");
        assert_eq!(untouched, "something else");
    }
}
