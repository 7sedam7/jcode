use super::*;

fn make_test_provider(fetched: Vec<String>) -> CopilotApiProvider {
    CopilotApiProvider {
        client: jcode_base::provider::shared_http_client(),
        model: Arc::new(RwLock::new(DEFAULT_MODEL.to_string())),
        github_token: "test-token".to_string(),
        model_specs: Arc::new(RwLock::new(CatalogSpecs::default())),
        fetched_models: Arc::new(RwLock::new(fetched)),
        catalog_source: Arc::new(RwLock::new(CatalogSource::Live)),
        account_type: Arc::new(RwLock::new(
            jcode_base::auth::copilot::CopilotAccountType::Unknown,
        )),
        session_id: "test-session".to_string(),
        machine_id: "test-machine".to_string(),
        init_ready: Arc::new(tokio::sync::Notify::new()),
        init_done: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        premium_mode: Arc::new(std::sync::atomic::AtomicU8::new(0)),
        user_turn_count: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        reasoning_effort: Arc::new(RwLock::new(None)),
        model_explicitly_selected: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        created_at: std::time::Instant::now(),
    }
}

#[test]
fn available_models_display_returns_fetched_when_populated() {
    let fetched = vec![
        "claude-opus-4.6".to_string(),
        "claude-sonnet-4.6".to_string(),
        "gpt-5.3-codex".to_string(),
        "gemini-3-pro-preview".to_string(),
    ];
    let provider = make_test_provider(fetched.clone());
    let display = provider.available_models_display();
    assert_eq!(display, fetched);
}

#[test]
fn no_catalog_means_no_models_offered() {
    // The old behavior served a hardcoded list here. That list named models no
    // account has (claude-opus-4.6-fast, gpt-5.4-pro, gpt-5.1-codex-max) and
    // omitted ones real accounts do have, so every entry was a coin flip
    // between working and an HTTP 400 `model_not_available_for_integrator`.
    let provider = make_test_provider(Vec::new());
    assert!(
        provider.available_models_display().is_empty(),
        "must not invent a catalog before /models answers"
    );
    assert!(provider.available_models_for_switching().is_empty());
    assert!(provider.available_models().is_empty());
}

#[test]
fn switching_list_matches_the_display_list() {
    // Anything offered in the picker must also be switchable, or the user can
    // see a model they cannot select.
    let fetched = vec!["claude-opus-4.7".to_string(), "gpt-5.6-sol".to_string()];
    let provider = make_test_provider(fetched.clone());
    assert_eq!(provider.available_models_display(), fetched);
    assert_eq!(provider.available_models_for_switching(), fetched);
}

#[tokio::test]
async fn prefetch_is_never_skipped_while_the_catalog_is_empty() {
    // The grace window deduplicates a startup burst. If it also suppressed the
    // first fetch, nothing else would retry and the picker would stay empty for
    // the whole session.
    let provider = make_test_provider(Vec::new());
    assert!(!provider.has_catalog());

    // With no catalog, prefetch must attempt detection. Detection here fails at
    // the bearer step (the token is fake), which is enough to prove it ran:
    // failure marks init done, whereas the grace-window skip returns before it.
    provider
        .init_done
        .store(false, std::sync::atomic::Ordering::Release);
    Provider::prefetch_models(&provider).await.unwrap();
    assert!(
        provider
            .init_done
            .load(std::sync::atomic::Ordering::Acquire),
        "prefetch must have run detection, not skipped it"
    );
}

#[tokio::test]
async fn prefetch_is_skipped_when_a_fresh_catalog_is_already_loaded() {
    let provider = make_test_provider(vec!["claude-opus-4.7".to_string()]);
    assert!(provider.has_catalog());

    provider
        .init_done
        .store(false, std::sync::atomic::Ordering::Release);
    Provider::prefetch_models(&provider).await.unwrap();
    assert!(
        !provider
            .init_done
            .load(std::sync::atomic::Ordering::Acquire),
        "a fresh catalog should suppress the duplicate startup fetch"
    );
}

#[test]
fn set_model_accepts_any_model_id() {
    let provider = make_test_provider(Vec::new());
    assert!(provider.set_model("claude-opus-4.6").is_ok());
    assert_eq!(provider.model(), "claude-opus-4.6");

    assert!(provider.set_model("some-new-model-2026").is_ok());
    assert_eq!(provider.model(), "some-new-model-2026");
}

#[test]
fn set_model_rejects_empty() {
    let provider = make_test_provider(Vec::new());
    assert!(provider.set_model("").is_err());
    assert!(provider.set_model("   ").is_err());
}

#[test]
fn gpt5_copilot_models_use_max_completion_tokens() {
    assert_eq!(
        CopilotApiProvider::max_token_parameter_for_model("gpt-5.4"),
        "max_completion_tokens"
    );
    assert_eq!(
        CopilotApiProvider::max_token_parameter_for_model(" GPT-5.4-pro "),
        "max_completion_tokens"
    );
    assert_eq!(
        CopilotApiProvider::max_token_parameter_for_model("gpt-5.3-codex"),
        "max_completion_tokens"
    );
}

#[test]
fn non_gpt5_copilot_models_keep_max_tokens() {
    assert_eq!(
        CopilotApiProvider::max_token_parameter_for_model("claude-sonnet-4.6"),
        "max_tokens"
    );
    assert_eq!(
        CopilotApiProvider::max_token_parameter_for_model("gemini-3-pro-preview"),
        "max_tokens"
    );
    assert_eq!(
        CopilotApiProvider::max_token_parameter_for_model("gpt-4.1"),
        "max_tokens"
    );
}

#[test]
fn context_window_handles_dot_and_dash_names() {
    // Copilot serves dotted ids (`claude-opus-4.6`); jcode passes hyphenated
    // ones around internally, and route suffixes like `-fast` may ride along.
    // Every spelling must land on the same window. Asserted against a published
    // catalog rather than the static table, because the table is only a
    // fallback and is wrong for most accounts.
    let _guard = ContextRegistryGuard::acquire();
    jcode_provider_core::record_copilot_catalog_context_limits(
        [
            ("claude-opus-4.6".to_string(), 200_000usize),
            ("claude-sonnet-4.6".to_string(), 1_000_000usize),
            ("gpt-5.4".to_string(), 1_050_000usize),
            ("gemini-3.1-pro-preview".to_string(), 1_000_000usize),
        ]
        .into_iter()
        .collect(),
    );

    let limit = |model: &str| {
        jcode_base::provider::context_limit_for_model_with_provider(model, Some("copilot"))
    };

    for spelling in ["claude-opus-4.6", "claude-opus-4-6", "claude-opus-4.6-fast"] {
        assert_eq!(limit(spelling), Some(200_000), "{spelling}");
    }
    for spelling in ["claude-sonnet-4.6", "claude-sonnet-4-6"] {
        assert_eq!(limit(spelling), Some(1_000_000), "{spelling}");
    }
    assert_eq!(limit("gpt-5.4"), Some(1_050_000));
    assert_eq!(limit("gemini-3.1-pro-preview"), Some(1_000_000));

    // A model the catalog never mentioned still resolves, via the fallback.
    assert_eq!(limit("unknown-model"), Some(128_000));
}

#[test]
fn has_credentials_returns_bool() {
    let _ = CopilotApiProvider::has_credentials();
}

#[test]
fn fork_preserves_fetched_models() {
    let fetched = vec!["model-a".to_string(), "model-b".to_string()];
    let provider = make_test_provider(fetched.clone());
    let forked = provider.fork();
    assert_eq!(forked.available_models_display(), fetched);
}

fn make_msg(role: Role, blocks: Vec<ContentBlock>) -> ChatMessage {
    ChatMessage {
        role,
        content: blocks,
        timestamp: None,
        tool_duration_ms: None,
    }
}

#[test]
fn build_messages_pairs_tool_use_with_tool_result() {
    let messages = vec![
        make_msg(
            Role::User,
            vec![ContentBlock::Text {
                text: "hello".into(),
                cache_control: None,
            }],
        ),
        make_msg(
            Role::Assistant,
            vec![
                ContentBlock::Text {
                    text: "let me check".into(),
                    cache_control: None,
                },
                ContentBlock::ToolUse {
                    id: "call_1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "echo hi"}),
                    thought_signature: None,
                },
            ],
        ),
        make_msg(
            Role::User,
            vec![ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
                content: "hi\n".into(),
                is_error: None,
            }],
        ),
    ];

    let built = CopilotApiProvider::build_messages("system prompt", &messages);

    assert_eq!(built.len(), 4);
    assert_eq!(built[0]["role"], "system");
    assert_eq!(built[1]["role"], "user");
    assert_eq!(built[1]["content"], "hello");
    assert_eq!(built[2]["role"], "assistant");
    assert!(built[2]["tool_calls"].is_array());
    assert_eq!(built[2]["tool_calls"][0]["id"], "call_1");
    assert_eq!(built[3]["role"], "tool");
    assert_eq!(built[3]["tool_call_id"], "call_1");
    assert_eq!(built[3]["content"], "hi\n");
}

#[test]
fn build_messages_injects_missing_tool_output() {
    let messages = vec![
        make_msg(
            Role::User,
            vec![ContentBlock::Text {
                text: "go".into(),
                cache_control: None,
            }],
        ),
        make_msg(
            Role::Assistant,
            vec![ContentBlock::ToolUse {
                id: "call_orphan".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "crash"}),
                thought_signature: None,
            }],
        ),
    ];

    let built = CopilotApiProvider::build_messages("", &messages);

    assert_eq!(built.len(), 3);
    assert_eq!(built[1]["role"], "assistant");
    assert_eq!(built[2]["role"], "tool");
    assert_eq!(built[2]["tool_call_id"], "call_orphan");
    assert!(built[2]["content"].as_str().unwrap().contains("missing"));
}

#[test]
fn build_messages_handles_batch_multiple_tool_calls() {
    let messages = vec![
        make_msg(
            Role::User,
            vec![ContentBlock::Text {
                text: "do things".into(),
                cache_control: None,
            }],
        ),
        make_msg(
            Role::Assistant,
            vec![
                ContentBlock::ToolUse {
                    id: "call_a".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "a"}),
                    thought_signature: None,
                },
                ContentBlock::ToolUse {
                    id: "call_b".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "b"}),
                    thought_signature: None,
                },
                ContentBlock::ToolUse {
                    id: "call_c".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "c"}),
                    thought_signature: None,
                },
            ],
        ),
        make_msg(
            Role::User,
            vec![
                ContentBlock::ToolResult {
                    tool_use_id: "call_a".into(),
                    content: "result_a".into(),
                    is_error: None,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "call_b".into(),
                    content: "result_b".into(),
                    is_error: None,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "call_c".into(),
                    content: "result_c".into(),
                    is_error: None,
                },
            ],
        ),
    ];

    let built = CopilotApiProvider::build_messages("", &messages);

    assert_eq!(built[0]["role"], "user");
    assert_eq!(built[1]["role"], "assistant");
    let tc = built[1]["tool_calls"].as_array().unwrap();
    assert_eq!(tc.len(), 3);

    assert_eq!(built[2]["role"], "tool");
    assert_eq!(built[2]["tool_call_id"], "call_a");
    assert_eq!(built[2]["content"], "result_a");
    assert_eq!(built[3]["role"], "tool");
    assert_eq!(built[3]["tool_call_id"], "call_b");
    assert_eq!(built[3]["content"], "result_b");
    assert_eq!(built[4]["role"], "tool");
    assert_eq!(built[4]["tool_call_id"], "call_c");
    assert_eq!(built[4]["content"], "result_c");
}

#[test]
fn build_messages_skips_empty_user_text() {
    let messages = vec![
        make_msg(
            Role::Assistant,
            vec![ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "read".into(),
                input: serde_json::json!({"file": "x"}),
                thought_signature: None,
            }],
        ),
        make_msg(
            Role::User,
            vec![ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
                content: "file content".into(),
                is_error: None,
            }],
        ),
    ];

    let built = CopilotApiProvider::build_messages("", &messages);

    assert_eq!(built.len(), 2);
    assert_eq!(built[0]["role"], "assistant");
    assert_eq!(built[1]["role"], "tool");
    assert_eq!(built[1]["content"], "file content");
}

#[test]
fn is_user_initiated_empty_messages() {
    let messages: Vec<ChatMessage> = vec![];
    assert!(CopilotApiProvider::is_user_initiated_raw(&messages));
}

#[test]
fn is_user_initiated_user_text_message() {
    let messages = vec![make_msg(
        Role::User,
        vec![ContentBlock::Text {
            text: "Hello".into(),
            cache_control: None,
        }],
    )];
    assert!(CopilotApiProvider::is_user_initiated_raw(&messages));
}

#[test]
fn is_user_initiated_tool_result_is_agent() {
    let messages = vec![
        make_msg(
            Role::User,
            vec![ContentBlock::Text {
                text: "Hello".into(),
                cache_control: None,
            }],
        ),
        make_msg(
            Role::Assistant,
            vec![ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "file_read".into(),
                input: json!({}),
                thought_signature: None,
            }],
        ),
        make_msg(
            Role::User,
            vec![ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
                content: "file content".into(),
                is_error: None,
            }],
        ),
    ];
    assert!(!CopilotApiProvider::is_user_initiated_raw(&messages));
}

#[test]
fn is_user_initiated_assistant_last_is_user_initiated() {
    let messages = vec![
        make_msg(
            Role::User,
            vec![ContentBlock::Text {
                text: "Hello".into(),
                cache_control: None,
            }],
        ),
        make_msg(
            Role::Assistant,
            vec![ContentBlock::Text {
                text: "Hi there".into(),
                cache_control: None,
            }],
        ),
    ];
    assert!(CopilotApiProvider::is_user_initiated_raw(&messages));
}

#[test]
fn is_user_initiated_tool_result_with_memory_injection() {
    let messages = vec![
        make_msg(
            Role::User,
            vec![ContentBlock::Text {
                text: "Hello".into(),
                cache_control: None,
            }],
        ),
        make_msg(
            Role::Assistant,
            vec![ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "bash".into(),
                input: json!({}),
                thought_signature: None,
            }],
        ),
        make_msg(
            Role::User,
            vec![ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
                content: "output".into(),
                is_error: None,
            }],
        ),
        make_msg(
            Role::User,
            vec![ContentBlock::Text {
                text: "<system-reminder>\nSome memory context\n</system-reminder>".into(),
                cache_control: None,
            }],
        ),
    ];
    assert!(!CopilotApiProvider::is_user_initiated_raw(&messages));
}

#[test]
fn is_user_initiated_user_text_after_tool_result_without_system_reminder() {
    let messages = vec![
        make_msg(
            Role::User,
            vec![ContentBlock::Text {
                text: "Hello".into(),
                cache_control: None,
            }],
        ),
        make_msg(
            Role::Assistant,
            vec![ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "bash".into(),
                input: json!({}),
                thought_signature: None,
            }],
        ),
        make_msg(
            Role::User,
            vec![ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
                content: "output".into(),
                is_error: None,
            }],
        ),
        make_msg(
            Role::User,
            vec![ContentBlock::Text {
                text: "Now do something else".into(),
                cache_control: None,
            }],
        ),
    ];
    assert!(CopilotApiProvider::is_user_initiated_raw(&messages));
}

#[test]
fn is_user_initiated_multiple_memory_injections_after_tool_result() {
    let messages = vec![
        make_msg(
            Role::Assistant,
            vec![ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "bash".into(),
                input: json!({}),
                thought_signature: None,
            }],
        ),
        make_msg(
            Role::User,
            vec![ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
                content: "output".into(),
                is_error: None,
            }],
        ),
        make_msg(
            Role::User,
            vec![ContentBlock::Text {
                text: "<system-reminder>\nMemory 1\n</system-reminder>".into(),
                cache_control: None,
            }],
        ),
        make_msg(
            Role::User,
            vec![ContentBlock::Text {
                text: "<system-reminder>\nMemory 2\n</system-reminder>".into(),
                cache_control: None,
            }],
        ),
    ];
    assert!(!CopilotApiProvider::is_user_initiated_raw(&messages));
}

#[test]
fn build_messages_sanitizes_tool_ids_with_dots() {
    let messages = vec![
        make_msg(
            Role::User,
            vec![ContentBlock::Text {
                text: "hello".into(),
                cache_control: None,
            }],
        ),
        make_msg(
            Role::Assistant,
            vec![ContentBlock::ToolUse {
                id: "chatcmpl-BF2xX.tool_call.0".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "echo hi"}),
                thought_signature: None,
            }],
        ),
        make_msg(
            Role::User,
            vec![ContentBlock::ToolResult {
                tool_use_id: "chatcmpl-BF2xX.tool_call.0".into(),
                content: "hi\n".into(),
                is_error: None,
            }],
        ),
    ];

    let built = CopilotApiProvider::build_messages("", &messages);

    let sanitized_id = "chatcmpl-BF2xX_tool_call_0";
    assert_eq!(built[1]["tool_calls"][0]["id"], sanitized_id);
    assert_eq!(built[2]["tool_call_id"], sanitized_id);
}

#[test]
fn build_messages_sanitizes_anthropic_style_ids() {
    let messages = vec![
        make_msg(
            Role::User,
            vec![ContentBlock::Text {
                text: "test".into(),
                cache_control: None,
            }],
        ),
        make_msg(
            Role::Assistant,
            vec![ContentBlock::ToolUse {
                id: "toolu_01XFDUDYJgAACzvnptvVer6u".into(),
                name: "read".into(),
                input: serde_json::json!({"file_path": "foo.rs"}),
                thought_signature: None,
            }],
        ),
        make_msg(
            Role::User,
            vec![ContentBlock::ToolResult {
                tool_use_id: "toolu_01XFDUDYJgAACzvnptvVer6u".into(),
                content: "file content".into(),
                is_error: None,
            }],
        ),
    ];

    let built = CopilotApiProvider::build_messages("", &messages);

    assert_eq!(
        built[1]["tool_calls"][0]["id"],
        "toolu_01XFDUDYJgAACzvnptvVer6u"
    );
    assert_eq!(built[2]["tool_call_id"], "toolu_01XFDUDYJgAACzvnptvVer6u");
}

#[test]
fn build_messages_sanitizes_missing_tool_output_ids() {
    let messages = vec![
        make_msg(
            Role::User,
            vec![ContentBlock::Text {
                text: "go".into(),
                cache_control: None,
            }],
        ),
        make_msg(
            Role::Assistant,
            vec![ContentBlock::ToolUse {
                id: "call.with.dots.orphan".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "crash"}),
                thought_signature: None,
            }],
        ),
    ];

    let built = CopilotApiProvider::build_messages("", &messages);

    assert_eq!(built[1]["tool_calls"][0]["id"], "call_with_dots_orphan");
    assert_eq!(built[2]["tool_call_id"], "call_with_dots_orphan");
}

/// Build a provider whose catalog describes `models` the way `/models` does.
///
/// Reasoning efforts are per-model and come from the API, so a test that does
/// not seed a catalog is testing the no-catalog path, not the effort logic.
fn provider_with_specs(model: &str, specs: &[(&str, &[&str])]) -> CopilotApiProvider {
    let infos: Vec<jcode_base::auth::copilot::CopilotModelInfo> = specs
        .iter()
        .map(|(id, efforts)| {
            serde_json::from_value(serde_json::json!({
                "id": id,
                "capabilities": {
                    "limits": { "max_context_window_tokens": 200_000, "max_output_tokens": 64_000 },
                    "supports": { "tool_calls": true, "reasoning_effort": efforts },
                },
            }))
            .expect("valid model info")
        })
        .collect();
    let provider = make_test_provider(specs.iter().map(|(id, _)| id.to_string()).collect());
    *provider.model_specs.write().unwrap() = CatalogSpecs::from_models(&infos);
    provider.set_model(model).unwrap();
    provider
}

/// Mirrors the live catalog: claude-sonnet-4.6 takes four efforts, gpt-5.6-sol
/// takes six including "none", and gpt-4.1 takes none at all.
fn efforts_provider(model: &str) -> CopilotApiProvider {
    provider_with_specs(
        model,
        &[
            ("claude-sonnet-4.6", &["low", "medium", "high", "max"]),
            (
                "gpt-5.6-sol",
                &["none", "low", "medium", "high", "xhigh", "max"],
            ),
            ("gpt-4.1", &[]),
        ],
    )
}

#[test]
fn reasoning_effort_serialized_when_set() {
    let provider = efforts_provider("claude-sonnet-4.6");
    Provider::set_reasoning_effort(&provider, "high").unwrap();
    let mut body = serde_json::json!({"model": "claude-sonnet-4.6"});
    provider.add_reasoning_effort_parameter(&mut body, "claude-sonnet-4.6");
    assert_eq!(body["reasoning_effort"], "high");
}

#[test]
fn reasoning_effort_absent_when_unset() {
    let provider = efforts_provider("claude-sonnet-4.6");
    let mut body = serde_json::json!({"model": "claude-sonnet-4.6"});
    provider.add_reasoning_effort_parameter(&mut body, "claude-sonnet-4.6");
    assert!(body.get("reasoning_effort").is_none());
    assert_eq!(Provider::reasoning_effort(&provider), None);
}

#[test]
fn each_model_accepts_exactly_the_efforts_it_advertises() {
    // The old code hardcoded one model and one effort list, so every other
    // reasoning model silently dropped the user's setting.
    let provider = efforts_provider("claude-sonnet-4.6");
    assert_eq!(
        Provider::available_efforts(&provider),
        vec!["low", "medium", "high", "max"]
    );
    for effort in ["low", "medium", "high", "max"] {
        Provider::set_reasoning_effort(&provider, effort).unwrap();
        assert_eq!(
            Provider::reasoning_effort(&provider).as_deref(),
            Some(effort)
        );
    }
    // Advertised by a *different* model, so it must be refused here.
    for bad in ["xhigh", "none", "banana", ""] {
        let err = Provider::set_reasoning_effort(&provider, bad).unwrap_err();
        assert!(
            err.to_string().contains("Unsupported reasoning effort"),
            "{bad}: {err}"
        );
    }

    let sol = efforts_provider("gpt-5.6-sol");
    assert_eq!(
        Provider::available_efforts(&sol),
        vec!["none", "low", "medium", "high", "xhigh", "max"]
    );
    Provider::set_reasoning_effort(&sol, "xhigh").unwrap();
}

#[test]
fn models_without_advertised_efforts_reject_them() {
    let provider = efforts_provider("gpt-4.1");
    assert!(Provider::available_efforts(&provider).is_empty());
    let err = Provider::set_reasoning_effort(&provider, "high").unwrap_err();
    assert!(
        err.to_string()
            .contains("does not accept a reasoning effort"),
        "{err}"
    );
    let mut body = serde_json::json!({"model": "gpt-4.1"});
    provider.add_reasoning_effort_parameter(&mut body, "gpt-4.1");
    assert!(body.get("reasoning_effort").is_none());
}

#[test]
fn effort_not_serialized_after_switching_to_a_model_without_efforts() {
    let provider = efforts_provider("claude-sonnet-4.6");
    Provider::set_reasoning_effort(&provider, "max").unwrap();
    provider.set_model("gpt-4.1").unwrap();
    let mut body = serde_json::json!({"model": "gpt-4.1"});
    provider.add_reasoning_effort_parameter(&mut body, "gpt-4.1");
    assert!(body.get("reasoning_effort").is_none());
    assert_eq!(Provider::reasoning_effort(&provider), None);
}

#[test]
fn efforts_are_empty_without_a_catalog() {
    // No catalog means no basis for claiming a model takes efforts.
    let provider = make_test_provider(Vec::new());
    provider.set_model("claude-sonnet-4.6").unwrap();
    assert!(Provider::available_efforts(&provider).is_empty());
    assert!(Provider::set_reasoning_effort(&provider, "high").is_err());
}

#[test]
fn fork_preserves_reasoning_effort() {
    let provider = efforts_provider("gpt-5.6-sol");
    Provider::set_reasoning_effort(&provider, "xhigh").unwrap();
    let forked = Provider::fork(&provider);
    assert_eq!(forked.reasoning_effort().as_deref(), Some("xhigh"));
}

/// The chat request path builds its own base URL, separate from auth, and used
/// to hardcode api.githubcopilot.com. An enterprise token is rejected there, so
/// this is the difference between working and a blanket 401.
#[test]
fn model_requests_use_the_enterprise_base_url() {
    let enterprise = {
        let _env = crate::errors::tests::DeploymentEnv::set("https://company.ghe.com/");
        request_base_url()
    };
    let dotcom = {
        let _env = crate::errors::tests::DeploymentEnv::set("");
        request_base_url()
    };

    assert_eq!(enterprise, "https://copilot-api.company.ghe.com");
    assert_eq!(dotcom, "https://api.githubcopilot.com");
}

/// Builds a provider whose catalog reports `window` for `model`.
fn provider_with_context_window(model: &str, window: usize) -> CopilotApiProvider {
    let info: jcode_base::auth::copilot::CopilotModelInfo = serde_json::from_value(json!({
        "id": model,
        "capabilities": {
            "limits": { "max_context_window_tokens": window },
            "supports": { "tool_calls": true },
        },
    }))
    .expect("valid model info");
    let provider = make_test_provider(vec![model.to_string()]);
    *provider.model_specs.write().unwrap() = CatalogSpecs::from_models(&[info]);
    provider.set_model(model).unwrap();
    provider
}

#[test]
fn the_live_catalog_beats_the_hardcoded_context_table() {
    // The static table says 200k for this model; the account's catalog says 1M.
    // The catalog is the only source that knows the real entitlement.
    assert_eq!(
        jcode_base::provider::context_limit_for_model_with_provider(
            "claude-opus-4.6",
            Some("copilot")
        ),
        Some(200_000),
        "precondition: the static table disagrees with the live catalog"
    );

    let provider = provider_with_context_window("claude-opus-4.6", 1_000_000);
    assert_eq!(provider.context_window(), 1_000_000);
}

#[test]
fn context_windows_differ_per_model_rather_than_being_one_global_number() {
    // The symptom this fixes: every model reporting the same 128k.
    for (model, window) in [
        ("claude-sonnet-4.6", 1_000_000usize),
        ("gpt-5.6-sol", 1_050_000),
        ("grok-4.5", 500_000),
        ("claude-haiku-4.5", 200_000),
        ("gpt-4o", 128_000),
    ] {
        let provider = provider_with_context_window(model, window);
        assert_eq!(provider.context_window(), window, "{model}");
    }
}

#[test]
fn an_unknown_model_still_falls_back_rather_than_reporting_zero() {
    let provider = make_test_provider(vec!["mystery-model".to_string()]);
    provider.write_model("mystery-model".to_string());
    assert!(provider.context_window() > 0);
}

#[test]
fn the_cached_catalog_round_trips_the_context_windows() {
    // Caching ids alone was why a relaunch fell back to 128k until the network
    // answered. The specs have to survive the round trip too.
    let info: jcode_base::auth::copilot::CopilotModelInfo = serde_json::from_value(json!({
        "id": "claude-sonnet-4.6",
        "capabilities": {
            "limits": { "max_context_window_tokens": 1_000_000, "max_output_tokens": 64_000 },
            "supports": { "tool_calls": true },
        },
    }))
    .unwrap();
    let cached = crate::startup::PersistedCopilotCatalog {
        models: vec!["claude-sonnet-4.6".to_string()],
        specs: CatalogSpecs::from_models(&[info]),
        fetched_at_rfc3339: "2026-01-01T00:00:00Z".to_string(),
    };

    let restored: crate::startup::PersistedCopilotCatalog =
        serde_json::from_str(&serde_json::to_string(&cached).unwrap()).unwrap();
    assert_eq!(
        restored.specs.context_window_for("claude-sonnet-4.6"),
        Some(1_000_000)
    );
    assert_eq!(
        restored.specs.max_output_tokens_for("claude-sonnet-4.6"),
        Some(64_000)
    );
}

#[test]
fn an_ids_only_cache_from_an_older_build_still_loads() {
    // Users upgrading in place have this file on disk already; refusing to parse
    // it would empty their picker.
    let legacy = r#"{"models":["gpt-4o"],"fetched_at_rfc3339":"2026-01-01T00:00:00Z"}"#;
    let restored: crate::startup::PersistedCopilotCatalog = serde_json::from_str(legacy).unwrap();
    assert_eq!(restored.models, vec!["gpt-4o".to_string()]);
    assert!(restored.specs.is_empty());
}

/// Serializes the tests that mutate the process-wide catalog registry.
static CONTEXT_REGISTRY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct ContextRegistryGuard(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);

impl ContextRegistryGuard {
    fn acquire() -> Self {
        let guard = CONTEXT_REGISTRY_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        jcode_provider_core::clear_copilot_catalog_context_limits();
        Self(guard)
    }
}

impl Drop for ContextRegistryGuard {
    fn drop(&mut self) {
        jcode_provider_core::clear_copilot_catalog_context_limits();
    }
}

#[test]
fn the_shared_context_lookup_prefers_the_live_catalog_over_the_static_table() {
    // This is the lookup the TUI, the compaction budget and the remote client
    // all use. It short-circuits to a static table for Copilot, and that table
    // caps the default model at 128k when this account is served 1M.
    let _guard = ContextRegistryGuard::acquire();

    assert_eq!(
        jcode_base::provider::context_limit_for_model_with_provider(
            "claude-sonnet-4.6",
            Some("copilot")
        ),
        Some(128_000),
        "precondition: the static table under-reports the default model"
    );

    jcode_provider_core::record_copilot_catalog_context_limits(
        [("claude-sonnet-4.6".to_string(), 1_000_000usize)]
            .into_iter()
            .collect(),
    );

    assert_eq!(
        jcode_base::provider::context_limit_for_model_with_provider(
            "claude-sonnet-4.6",
            Some("copilot")
        ),
        Some(1_000_000)
    );
}

#[test]
fn the_shared_lookup_matches_dotted_and_hyphenated_spellings() {
    // Copilot serves `claude-sonnet-4.6`; jcode passes `claude-sonnet-4-6`
    // around internally. Both must resolve to the account's real window.
    let _guard = ContextRegistryGuard::acquire();
    jcode_provider_core::record_copilot_catalog_context_limits(
        [("claude-sonnet-4.6".to_string(), 1_000_000usize)]
            .into_iter()
            .collect(),
    );

    for spelling in [
        "claude-sonnet-4.6",
        "claude-sonnet-4-6",
        "Claude-Sonnet-4.6",
    ] {
        assert_eq!(
            jcode_base::provider::context_limit_for_model_with_provider(spelling, Some("copilot")),
            Some(1_000_000),
            "{spelling}"
        );
    }
}

#[test]
fn a_model_missing_from_the_catalog_still_falls_back_to_the_static_table() {
    let _guard = ContextRegistryGuard::acquire();
    jcode_provider_core::record_copilot_catalog_context_limits(
        [("claude-sonnet-4.6".to_string(), 1_000_000usize)]
            .into_iter()
            .collect(),
    );

    assert_eq!(
        jcode_base::provider::context_limit_for_model_with_provider(
            "claude-opus-4.6",
            Some("copilot")
        ),
        Some(200_000),
        "a model the catalog did not mention must keep its previous answer"
    );
}

#[test]
fn an_empty_catalog_does_not_wipe_the_previous_one() {
    // A failed refresh must not downgrade every model back to 128k.
    let _guard = ContextRegistryGuard::acquire();
    jcode_provider_core::record_copilot_catalog_context_limits(
        [("claude-sonnet-4.6".to_string(), 1_000_000usize)]
            .into_iter()
            .collect(),
    );
    jcode_provider_core::record_copilot_catalog_context_limits(std::collections::HashMap::new());

    assert_eq!(
        jcode_provider_core::copilot_catalog_context_limit("claude-sonnet-4.6"),
        Some(1_000_000)
    );
}

#[test]
fn tier_detection_does_not_overwrite_an_explicitly_selected_model() {
    // Tier detection is spawned at construction and lands *after* `--model`,
    // the `/model` picker, or session restore has called `set_model`. It used to
    // write the catalog default unconditionally, so every turn in the session
    // went to that one model no matter what the user picked.
    let catalog = vec!["claude-opus-4.6".to_string(), "grok-4.5".to_string()];
    let provider = make_test_provider(catalog.clone());

    provider.set_model("grok-4.5").expect("set_model");
    provider.apply_catalog_default("claude-opus-4.6".to_string(), &catalog);

    assert_eq!(provider.model(), "grok-4.5");
}

#[test]
fn tier_detection_still_installs_the_default_when_nothing_was_selected() {
    let catalog = vec!["claude-opus-4.6".to_string(), "grok-4.5".to_string()];
    let provider = make_test_provider(catalog.clone());

    provider.apply_catalog_default("claude-opus-4.6".to_string(), &catalog);

    assert_eq!(provider.model(), "claude-opus-4.6");
}

#[test]
fn a_prefixed_selection_survives_tier_detection_too() {
    // Session restore hands this runtime `copilot:<model>`; the prefix is
    // stripped by `set_model`, and the stripped choice must still be honored.
    let catalog = vec!["claude-opus-4.6".to_string(), "gpt-5.5".to_string()];
    let provider = make_test_provider(catalog.clone());

    provider.set_model("copilot:gpt-5.5").expect("set_model");
    provider.apply_catalog_default("claude-opus-4.6".to_string(), &catalog);

    assert_eq!(provider.model(), "gpt-5.5");
}

#[test]
fn a_forked_provider_keeps_the_selection_it_inherited() {
    // `fork` gives the child its own model slot; if the selection flag were not
    // carried across, a fork's tier detection would clobber the choice again.
    let catalog = vec!["claude-opus-4.6".to_string(), "grok-4.5".to_string()];
    let provider = make_test_provider(catalog.clone());
    provider.set_model("grok-4.5").expect("set_model");

    let forked = provider.fork();
    assert_eq!(forked.model(), "grok-4.5");
}

#[test]
fn forgetting_a_stale_spec_forces_the_next_lookup_to_refetch() {
    // Copilot answers HTTP 400 `unsupported_api_for_model` when a model is sent
    // to an endpoint it does not serve. That means our catalog copy is stale,
    // not that the request is impossible, so the runtime forgets the entry and
    // retries; the miss is what triggers the refetch.
    let info: jcode_base::auth::copilot::CopilotModelInfo = serde_json::from_value(json!({
        "id": "gpt-5.6-terra",
        "supported_endpoints": ["/responses", "ws:/responses"],
        "capabilities": {
            "limits": { "max_context_window_tokens": 1_050_000, "max_output_tokens": 128_000 },
            "supports": { "tool_calls": true },
        },
    }))
    .expect("model info");

    let mut specs = crate::catalog::CatalogSpecs::from_models(&[info]);
    assert_eq!(
        specs.endpoint_for("gpt-5.6-terra"),
        jcode_base::auth::copilot::CopilotEndpoint::Responses
    );

    assert!(specs.forget("gpt-5.6-terra"));
    // Forgotten, so the lookup now misses. The miss is the refetch trigger; the
    // bare fallback on its own would be the wrong endpoint for this model.
    assert!(specs.get("gpt-5.6-terra").is_none());
    // Forgetting twice reports false, which is what stops the retry from
    // looping when the spec was already absent.
    assert!(!specs.forget("gpt-5.6-terra"));
}

#[test]
fn a_responses_only_model_is_never_routed_to_chat_completions_when_described() {
    // The regression that produced the user-visible 400: `/responses`-only
    // models (gpt-5.5, gpt-5.6-sol, gpt-5.6-terra) fell back to
    // `/chat/completions` whenever the catalog had not described them.
    let models: Vec<jcode_base::auth::copilot::CopilotModelInfo> =
        ["gpt-5.5", "gpt-5.6-sol", "gpt-5.6-terra"]
            .iter()
            .map(|id| {
                serde_json::from_value(json!({
                    "id": id,
                    "supported_endpoints": ["/responses", "ws:/responses"],
                    "capabilities": {
                        "limits": {
                            "max_context_window_tokens": 1_050_000,
                            "max_output_tokens": 128_000,
                        },
                        "supports": { "tool_calls": true },
                    },
                }))
                .expect("model info")
            })
            .collect();

    let specs = crate::catalog::CatalogSpecs::from_models(&models);
    for model in ["gpt-5.5", "gpt-5.6-sol", "gpt-5.6-terra"] {
        assert_eq!(
            specs.endpoint_for(model),
            jcode_base::auth::copilot::CopilotEndpoint::Responses,
            "{model} must route to /responses"
        );
    }
}
