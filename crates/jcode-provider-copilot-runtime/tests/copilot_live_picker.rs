//! Opt-in live check that the model picker offers exactly the account's catalog.
//!
//! The picker list is what `/models` shows the user. It must come from the live
//! `/models` response, because the reachable model set depends on the OAuth app
//! the token was minted under. A hardcoded list will always be wrong for some
//! account: it names models the account cannot reach (HTTP 400
//! `model_not_available_for_integrator` when picked) and hides ones it can.

use jcode_provider_core::Provider;

#[tokio::test]
#[ignore = "hits the live Copilot API"]
async fn picker_shows_the_live_catalog_not_a_hardcoded_list() {
    let token = std::env::var("COPILOT_LIVE_TOKEN").expect("COPILOT_LIVE_TOKEN");

    let p = jcode_provider_copilot_runtime::CopilotApiProvider::new_with_token(token.clone());
    let before = p.available_models_display();
    eprintln!("BEFORE detect ({}): {:?}", before.len(), before);

    p.detect_tier_and_set_default().await;
    let after = p.available_models_display();
    eprintln!("AFTER detect ({}): {:?}", after.len(), after);
    eprintln!("default model -> {}", p.model());
    eprintln!("catalog detail -> {:?}", p.model_catalog_detail());

    let live = jcode_base::auth::copilot::fetch_available_models(
        &jcode_provider_core::shared_http_client(),
        &token,
    )
    .await
    .expect("live catalog");
    let picker: Vec<String> = live
        .iter()
        .filter(|m| m.model_picker_enabled)
        .map(|m| m.id.clone())
        .collect();

    assert_eq!(after, picker, "picker must mirror the live catalog exactly");
    for model in &after {
        assert!(
            live.iter().any(|m| &m.id == model),
            "picker offers '{model}', which the account cannot reach"
        );
    }
}

/// The catalog must actually describe reasoning efforts, or jcode silently
/// drops the user's `/effort` setting on every model but one.
#[tokio::test]
#[ignore = "hits the live Copilot API"]
async fn reasoning_efforts_come_from_the_catalog() {
    let token = std::env::var("COPILOT_LIVE_TOKEN").expect("COPILOT_LIVE_TOKEN");
    let p = jcode_provider_copilot_runtime::CopilotApiProvider::new_with_token(token);
    p.detect_tier_and_set_default().await;

    // Every reasoning model advertises its own vocabulary; asserting a fixed
    // list would just re-encode the bug this replaced.
    let mut described = 0;
    for model in p.available_models_display() {
        p.set_model(&model).expect("set model");
        let efforts = p.available_efforts();
        if efforts.is_empty() {
            continue;
        }
        described += 1;
        eprintln!("{model}: {efforts:?}");
        for effort in &efforts {
            p.set_reasoning_effort(effort)
                .unwrap_or_else(|e| panic!("{model} advertised '{effort}' but rejected it: {e}"));
        }
        assert!(
            p.set_reasoning_effort("definitely-not-an-effort").is_err(),
            "{model} must reject an effort outside its advertised set"
        );
    }
    assert!(
        described > 1,
        "only {described} model(s) reported reasoning efforts; the catalog lists many"
    );
}

/// Non-chat catalog entries (embeddings) must never reach the picker.
#[tokio::test]
#[ignore = "hits the live Copilot API"]
async fn embedding_models_are_not_offered_as_chat_models() {
    let token = std::env::var("COPILOT_LIVE_TOKEN").expect("COPILOT_LIVE_TOKEN");
    let p = jcode_provider_copilot_runtime::CopilotApiProvider::new_with_token(token);
    p.detect_tier_and_set_default().await;

    for model in p.available_models_display() {
        assert!(
            !model.contains("embedding"),
            "'{model}' cannot serve an agent turn but was offered"
        );
    }
}

#[tokio::test]
#[ignore = "hits the live Copilot API"]
async fn context_windows_come_from_the_account_catalog() {
    // The bug this guards: every model reporting the same fallback number
    // because the catalog's limits were never consulted.
    let token = std::env::var("COPILOT_LIVE_TOKEN").expect("COPILOT_LIVE_TOKEN");
    let p = jcode_provider_copilot_runtime::CopilotApiProvider::new_with_token(token.clone());
    p.detect_tier_and_set_default().await;

    let mut windows = Vec::new();
    for model in p.available_models_for_switching() {
        p.set_model(&model).expect("catalog model is settable");
        let window = p.context_window();
        eprintln!("{model} -> {window}");
        assert!(window > 0, "{model} reported no context window");
        windows.push(window);
    }

    assert!(!windows.is_empty(), "catalog was empty");
    windows.sort_unstable();
    windows.dedup();
    assert!(
        windows.len() > 1,
        "every model reported the same window {windows:?}, which means the \
         catalog limits are being ignored"
    );
    assert!(
        windows.iter().any(|w| *w > 128_000),
        "no model exceeded the 128k fallback, so the catalog is not being read: {windows:?}"
    );
}

#[tokio::test]
#[ignore = "hits the live Copilot API"]
async fn the_shared_context_lookup_reports_real_windows_after_startup() {
    // `context_window()` on the provider was already right; the number the user
    // actually sees comes from this free function, which short-circuited to a
    // static table that answers 128k for almost everything.
    let token = std::env::var("COPILOT_LIVE_TOKEN").expect("COPILOT_LIVE_TOKEN");
    let p = jcode_provider_copilot_runtime::CopilotApiProvider::new_with_token(token);
    p.detect_tier_and_set_default().await;

    let mut seen = Vec::new();
    for model in p.available_models_for_switching() {
        let shared =
            jcode_base::provider::context_limit_for_model_with_provider(&model, Some("copilot"))
                .expect("copilot always resolves a limit");
        p.set_model(&model).unwrap();
        let direct = p.context_window();
        eprintln!("{model}: shared={shared} direct={direct}");
        assert_eq!(
            shared, direct,
            "{model}: the shared lookup disagrees with the provider"
        );
        seen.push(shared);
    }

    seen.sort_unstable();
    seen.dedup();
    assert!(
        seen.len() > 1 && seen.iter().any(|w| *w > 128_000),
        "the shared lookup is still answering from the static table: {seen:?}"
    );
}
