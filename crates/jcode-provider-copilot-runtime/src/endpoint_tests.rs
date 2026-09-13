use super::*;
use futures::StreamExt;
use jcode_message_types::{ContentBlock, Role};

#[test]
fn credential_reload_updates_shared_forks_and_clears_the_old_endpoint() {
    let sandbox = jcode_base::auth::test_sandbox::AuthTestSandbox::new().unwrap();
    let hosts_path = jcode_base::auth::copilot::saved_hosts_path();
    std::fs::create_dir_all(hosts_path.parent().unwrap()).unwrap();
    std::fs::write(
        &hosts_path,
        r#"{"github.com":{"oauth_token":"fresh-token","user":"test"}}"#,
    )
    .unwrap();
    jcode_base::auth::copilot::trust_external_auth_source(
        jcode_base::auth::copilot::ExternalCopilotAuthSource::HostsJson,
    )
    .unwrap();

    let provider = tests::make_test_provider(vec!["stale-model".to_string()]);
    jcode_base::auth::copilot_enterprise::record_discovered_api_base_for(
        "test-token",
        "https://old.example.test",
    );
    let fork = provider.fork();
    Provider::reload_credentials(&provider);

    assert_eq!(
        provider.github_token.read().unwrap().as_str(),
        "fresh-token"
    );
    assert_eq!(
        jcode_base::auth::copilot_enterprise::discovered_api_base_for("test-token"),
        None
    );
    assert!(Arc::strong_count(&provider.github_token) >= 2);
    drop(fork);
    drop(sandbox);
}

#[test]
fn auth_invalidation_forces_a_fresh_catalog_fetch() {
    let provider = tests::make_test_provider(vec!["stale-model".to_string()]);
    assert!(provider.has_catalog());
    provider.clear_catalog();
    assert!(!provider.has_catalog());
    assert!(provider.model_specs.read().unwrap().is_empty());
    assert_eq!(
        *provider.catalog_source.read().unwrap(),
        CatalogSource::None
    );
    assert_eq!(
        *provider.account_type.read().unwrap(),
        copilot_auth::CopilotAccountType::Unknown
    );
}

#[test]
fn missing_replacement_credentials_disable_the_old_account() {
    let sandbox = jcode_base::auth::test_sandbox::AuthTestSandbox::new().unwrap();
    let provider = tests::make_test_provider(Vec::new());

    Provider::reload_credentials(&provider);

    assert!(provider.github_token.read().unwrap().is_empty());
    drop(sandbox);
}

#[test]
fn superseded_credential_cannot_commit_catalog_state() {
    let provider = tests::make_test_provider(Vec::new());
    *provider.github_token.write().unwrap() = "new-token".to_string();

    let committed = provider.with_current_credential("old-token", 1, || {
        provider
            .fetched_models
            .write()
            .unwrap()
            .push("stale-model".to_string());
    });

    assert!(committed.is_none());
    assert!(provider.fetched_models.read().unwrap().is_empty());
}

#[test]
fn same_token_relogin_invalidates_an_older_catalog_commit() {
    let provider = tests::make_test_provider(Vec::new());
    let generation = provider.credential_generation();
    provider.invalidate_credential_generation();

    let committed = provider.with_current_credential("test-token", generation, || {
        provider
            .fetched_models
            .write()
            .unwrap()
            .push("stale-model".to_string());
    });

    assert!(committed.is_none());
    assert!(provider.fetched_models.read().unwrap().is_empty());
}

#[test]
fn persisted_catalog_is_loaded_only_for_the_credential_that_fetched_it() {
    let sandbox = jcode_base::auth::test_sandbox::AuthTestSandbox::new().unwrap();
    let path = jcode_base::storage::app_config_dir()
        .unwrap()
        .join("copilot_models_cache.json");
    let write_cache = |token: &str| {
        let cache = crate::startup::PersistedCopilotCatalog {
            models: vec!["account-a-model".to_string()],
            specs: CatalogSpecs::default(),
            credential_key: copilot_auth_enterprise::token_cache_key(token),
            fetched_at_rfc3339: "2026-09-13T00:00:00Z".to_string(),
        };
        jcode_base::storage::write_json(&path, &cache).unwrap();
    };

    write_cache("account-a-token");
    let other = CopilotApiProvider::new_with_token("account-b-token".to_string());
    assert!(other.available_models_display().is_empty());

    write_cache("account-b-token");
    let matching = CopilotApiProvider::new_with_token("account-b-token".to_string());
    assert_eq!(
        matching.available_models_display(),
        vec!["account-a-model".to_string()]
    );
    drop(sandbox);
}

#[test]
fn forbidden_is_not_misreported_as_expired_authentication() {
    assert!(CopilotApiProvider::is_auth_error(
        reqwest::StatusCode::UNAUTHORIZED
    ));
    assert!(!CopilotApiProvider::is_auth_error(
        reqwest::StatusCode::FORBIDDEN
    ));
}

#[tokio::test]
#[ignore = "hits the live GitHub Copilot API"]
async fn fresh_headless_runtime_discovers_endpoint_before_first_inference() {
    let token = std::env::var("COPILOT_LIVE_TOKEN").expect("COPILOT_LIVE_TOKEN");
    copilot_auth_enterprise::clear_discovered_api_base_for(&token);
    let provider = CopilotApiProvider::new_with_token(token.clone());
    provider.complete_init_without_tier_detection();
    provider.set_model("gpt-5.6-sol").unwrap();
    let message = ChatMessage {
        role: Role::User,
        content: vec![ContentBlock::Text {
            text: "Reply with exactly: FRESH_ENDPOINT_OK".to_string(),
            cache_control: None,
        }],
        timestamp: None,
        tool_duration_ms: None,
    };

    let mut stream = provider.complete(&[message], &[], "", None).await.unwrap();
    let mut text = String::new();
    let mut ended = false;
    while let Some(event) = stream.next().await {
        match event.unwrap() {
            StreamEvent::TextDelta(delta) => text.push_str(&delta),
            StreamEvent::MessageEnd { .. } => ended = true,
            _ => {}
        }
    }

    assert!(ended, "Copilot stream did not finish");
    assert!(text.contains("FRESH_ENDPOINT_OK"), "response was {text:?}");
    assert!(
        copilot_auth_enterprise::discovered_api_base_for(&token).is_some(),
        "first inference must discover and cache the seat endpoint"
    );
}
