use super::*;

#[test]
fn fork_shares_catalog_but_isolates_session_state() {
    let infos: Vec<jcode_base::auth::copilot::CopilotModelInfo> = ["model-a", "model-b", "model-c"]
        .into_iter()
        .map(|id| {
            serde_json::from_value(serde_json::json!({
                "id": id,
                "capabilities": {"supports": {"reasoning_effort": ["low", "high"]}},
            }))
            .unwrap()
        })
        .collect();
    let provider = CopilotApiProvider {
        client: jcode_base::provider::shared_http_client(),
        model: Arc::new(RwLock::new("model-a".to_string())),
        github_token: "test-token".to_string(),
        fetched_models: Arc::new(RwLock::new(infos.iter().map(|m| m.id.clone()).collect())),
        model_specs: Arc::new(RwLock::new(CatalogSpecs::from_models(&infos))),
        catalog_source: Arc::new(RwLock::new(CatalogSource::Live)),
        account_type: Arc::new(RwLock::new(copilot_auth::CopilotAccountType::Unknown)),
        session_id: "test-session".to_string(),
        machine_id: "test-machine".to_string(),
        init_ready: Arc::new(tokio::sync::Notify::new()),
        init_done: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        premium_mode: Arc::new(std::sync::atomic::AtomicU8::new(
            PremiumMode::OnePerSession as u8,
        )),
        user_turn_count: Arc::new(std::sync::atomic::AtomicU64::new(3)),
        reasoning_effort: Arc::new(RwLock::new(Some("low".to_string()))),
        model_explicitly_selected: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        created_at: std::time::Instant::now(),
    };
    let forked = provider.fork();

    assert_eq!(Arc::strong_count(&provider.fetched_models), 2);
    assert_eq!(Arc::strong_count(&provider.model_specs), 2);
    assert_eq!(Arc::strong_count(&provider.catalog_source), 2);
    assert_eq!(Arc::strong_count(&provider.model), 1);
    assert_eq!(Arc::strong_count(&provider.premium_mode), 1);
    assert_eq!(Arc::strong_count(&provider.user_turn_count), 1);
    assert_eq!(Arc::strong_count(&provider.reasoning_effort), 1);
    assert_eq!(Arc::strong_count(&provider.model_explicitly_selected), 1);

    provider
        .fetched_models
        .write()
        .unwrap()
        .push("model-d".to_string());
    assert!(
        forked
            .available_models_display()
            .iter()
            .any(|model| model == "model-d")
    );

    forked.set_model("model-c").unwrap();
    forked.set_premium_mode(PremiumMode::Zero);
    forked.set_reasoning_effort("high").unwrap();
    assert_eq!(provider.model(), "model-a");
    assert_eq!(provider.get_premium_mode(), PremiumMode::OnePerSession);
    assert_eq!(
        provider
            .user_turn_count
            .load(std::sync::atomic::Ordering::Relaxed),
        3
    );
    assert_eq!(provider.current_reasoning_effort().as_deref(), Some("low"));
}
