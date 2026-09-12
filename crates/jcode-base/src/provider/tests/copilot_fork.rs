#[test]
fn multi_provider_fork_isolates_copilot_model_selection() {
    with_clean_provider_test_env(|| {
        let copilot = test_copilot_runtime();
        let parent_model = copilot.model();
        let worker_model = copilot::FALLBACK_MODELS
            .iter()
            .copied()
            .find(|model| *model != parent_model)
            .expect("Copilot test catalog should contain a second model");
        let provider = MultiProvider {
            claude: RwLock::new(None),
            anthropic: RwLock::new(None),
            openai: RwLock::new(None),
            copilot_api: RwLock::new(Some(Arc::clone(&copilot))),
            antigravity: RwLock::new(None),
            gemini: RwLock::new(None),
            cursor: RwLock::new(None),
            bedrock: RwLock::new(None),
            openrouter: RwLock::new(None),
            openai_compatible_profiles: RwLock::new(std::collections::HashMap::new()),
            active_openai_compatible_profile: RwLock::new(None),
            active: RwLock::new(ActiveProvider::Copilot),
            use_claude_cli: false,
            startup_notices: RwLock::new(Vec::new()),
            initial_provider: Some(ActiveProvider::Copilot),
            routes_memo: std::sync::Mutex::new(None),
            post_auth_refreshes_pending: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };

        let fork = provider.fork();
        fork.set_model(&format!("copilot:{worker_model}")).unwrap();

        assert_eq!(fork.model(), worker_model);
        assert_eq!(copilot.model(), parent_model);
        assert_eq!(provider.model(), parent_model);
    });
}
