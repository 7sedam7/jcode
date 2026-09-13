#[derive(Clone)]
struct BlockingCopilotRefreshProvider {
    started: Arc<std::sync::atomic::AtomicBool>,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl Provider for BlockingCopilotRefreshProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        unimplemented!("BlockingCopilotRefreshProvider")
    }

    fn name(&self) -> &str {
        "copilot"
    }

    fn model(&self) -> String {
        "gpt-5.6-sol".to_string()
    }

    fn set_model(&self, _model: &str) -> Result<()> {
        Ok(())
    }

    fn available_models_display(&self) -> Vec<String> {
        vec![self.model()]
    }

    fn available_models_for_switching(&self) -> Vec<String> {
        self.available_models_display()
    }

    fn model_routes(&self) -> Vec<ModelRoute> {
        vec![ModelRoute {
            model: self.model(),
            provider: "GitHub Copilot".to_string(),
            api_method: "copilot".to_string(),
            available: true,
            detail: String::new(),
            cheapness: None,
        }]
    }

    async fn prefetch_models(&self) -> Result<()> {
        self.started
            .store(true, std::sync::atomic::Ordering::Release);
        self.release.notified().await;
        Ok(())
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }
}

#[test]
fn test_on_auth_changed_tracks_copilot_endpoint_and_catalog_refresh() {
    with_clean_provider_test_env(|| {
        with_env_var("GITHUB_TOKEN", "gho_test_token", || {
            crate::auth::AuthStatus::invalidate_cache();
            let runtime = enter_test_runtime();
            let enter = runtime.enter();
            let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let release = Arc::new(tokio::sync::Notify::new());
            let factory_started = Arc::clone(&started);
            let factory_release = Arc::clone(&release);
            external::register_external_provider(external::COPILOT_RUNTIME, move || {
                Arc::new(BlockingCopilotRefreshProvider {
                    started: Arc::clone(&factory_started),
                    release: Arc::clone(&factory_release),
                })
            });

            let provider = MultiProvider {
                claude: RwLock::new(None),
                anthropic: RwLock::new(None),
                openai: RwLock::new(None),
                copilot_api: RwLock::new(None),
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

            provider.on_auth_changed();
            runtime.block_on(async {
                tokio::time::timeout(std::time::Duration::from_secs(1), async {
                    while !started.load(std::sync::atomic::Ordering::Acquire) {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("Copilot post-auth prefetch should start");
                assert!(provider.auth_model_refresh_pending());

                release.notify_waiters();
                tokio::time::timeout(std::time::Duration::from_secs(1), async {
                    while provider.auth_model_refresh_pending() {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("Copilot post-auth prefetch should finish");
            });
            drop(enter);
        })
    });
}
