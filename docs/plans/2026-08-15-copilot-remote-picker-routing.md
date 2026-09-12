# Copilot Remote Picker Routing Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Preserve authoritative provider ownership across the server/TUI boundary so Copilot models such as `gpt-5.6-sol` never become OpenAI routes and canonical model commands always reach the server.

**Architecture:** The server will include compact authoritative routes in the initial History catalog and track only catalog data actually delivered to each connection. Names-only remote catalogs will use neutral placeholders rather than model-family provider inference. Available structured routes continue to use `SetRoute`; a complete canonical command bypasses unavailable or placeholder preview rows and uses the server's canonical `SetModel` parser.

**Tech Stack:** Rust, Tokio, Serde, JCode protocol, Ratatui/Crossterm, Cargo tests.

**Repository constraints:** Work on the existing branch and worktree. Do not commit.

---

### Task 1: Send authoritative compact routes in initial History

**Files:**
- Modify: `crates/jcode-app-core/src/server/client_state.rs`
- Test: `crates/jcode-app-core/src/server/client_state_tests.rs`

**Step 1: Extend the test provider with a routed Copilot catalog**

Make `MockProvider` return:

```rust
fn available_models_display(&self) -> Vec<String> {
    vec!["gpt-5.6-sol".to_string()]
}

fn model_routes(&self) -> Vec<crate::provider::ModelRoute> {
    vec![crate::provider::ModelRoute {
        model: "gpt-5.6-sol".to_string(),
        provider: "Copilot".to_string(),
        api_method: "copilot".to_string(),
        available: true,
        detail: "optional detail".repeat(32),
        cheapness: None,
    }]
}
```

**Step 2: Write the failing History test**

Call `handle_get_history` with an unlocked agent and assert the decoded
`ServerEvent::History` contains one route with:

```rust
assert_eq!(available_model_routes[0].model, "gpt-5.6-sol");
assert_eq!(available_model_routes[0].provider, "Copilot");
assert_eq!(available_model_routes[0].api_method, "copilot");
assert!(available_model_routes[0].available);
assert!(available_model_routes[0].detail.is_empty());
assert!(available_model_routes[0].cheapness.is_none());
```

**Step 3: Run the test and verify it fails**

```bash
cargo test -p jcode-app-core handle_get_history_includes_compact_model_routes -- --exact --nocapture
```

Expected: FAIL because normal History currently sets
`available_model_routes = Vec::new()`.

**Step 4: Add a shared compact-route helper**

Move compact route projection into a small reusable function near the server
catalog helpers:

```rust
fn compact_model_routes(routes: Vec<ModelRoute>) -> Vec<ModelRoute> {
    routes
        .into_iter()
        .map(|mut route| {
            route.detail.clear();
            route.cheapness = None;
            route
        })
        .collect()
}
```

Use it in both initial History construction and
`compact_available_models_event`. Do not duplicate the projection logic.

`available_models_display()` already materializes the provider route memo, so
the following `model_routes()` call reuses that catalog instead of performing
independent provider discovery.

**Step 5: Run the focused server tests**

```bash
cargo test -p jcode-app-core handle_get_history_includes_compact_model_routes -- --exact --nocapture
cargo test -p jcode-app-core available_models_event -- --nocapture
```

Expected: PASS.

### Task 2: Make connection dedup reflect the catalog actually delivered

**Files:**
- Modify: `crates/jcode-app-core/src/server/client_lifecycle.rs`
- Test: `crates/jcode-app-core/src/server/client_lifecycle_tests.rs`

**Step 1: Write a failing dedup-state test**

Extract a pure helper that derives the post-History dedup key from the History
catalog fields. Test these cases:

```rust
// Names-only History must not equal the routed update.
assert_ne!(history_key, routed_update_key);

// Route-bearing History may equal the same routed update after compact
// normalization.
assert_eq!(routed_history_key, routed_update_key);
```

The key must describe only the models and compact routes actually serialized in
History.

**Step 2: Run the test and verify it fails**

```bash
cargo test -p jcode-app-core history_catalog_dedup_key_tracks_delivered_routes -- --exact --nocapture
```

Expected: FAIL because the connection currently calls
`try_available_models_snapshot(&agent)` after History, which records the full
provider snapshot regardless of what History contained.

**Step 3: Store the delivered History key**

Have History construction return or expose the compact catalog snapshot it
serialized. Set `last_available_models_snapshot` from that snapshot rather than
re-reading the full agent catalog.

Do not set the full routed dedup key after names-only or route-empty fallback
History.

**Step 4: Run the focused lifecycle tests**

```bash
cargo test -p jcode-app-core history_catalog_dedup_key_tracks_delivered_routes -- --exact --nocapture
cargo test -p jcode-app-core client_lifecycle -- --nocapture
```

Expected: PASS.

### Task 3: Stop provider inference for names-only aggregate catalogs

**Files:**
- Modify: `crates/jcode-base/src/provider/catalog_routes.rs`
- Modify: `crates/jcode-tui/src/tui/app/inline_interactive.rs`
- Test: `crates/jcode-base/src/provider/tests/model_resolution.rs`
- Test: `crates/jcode-tui/src/tui/app/tests/remote_startup_input_02/part_01.rs`

**Step 1: Write the failing route-classification test**

For an aggregate names-only remote catalog containing `gpt-5.6-sol`, assert the
fallback does not produce an OpenAI route:

```rust
let routes = remote_names_only_placeholder_routes(&["gpt-5.6-sol".to_string()]);
assert_eq!(routes.len(), 1);
assert_eq!(routes[0].model, "gpt-5.6-sol");
assert_eq!(routes[0].provider, "Remote server");
assert_eq!(routes[0].api_method, "remote-catalog");
assert!(!routes[0].available);
assert!(routes[0].detail.contains("route details"));
```

Also assert no route has provider `OpenAI`, `Anthropic`, `Gemini`, or
`Copilot`. Names alone cannot authoritatively select any of them.

**Step 2: Run the test and verify it fails**

```bash
cargo test -p jcode-base names_only_remote_catalog_does_not_infer_provider -- --exact --nocapture
```

Expected: FAIL because `remote_model_routes_fallback` currently classifies GPT
IDs as OpenAI before considering Copilot.

**Step 3: Add neutral placeholder construction**

Add a dedicated function for aggregate names-only snapshots. It must create
`remote-catalog` placeholders and must not call `provider_for_model`.

Use this function only when the server supplied model names with zero routes.
Keep `remote_model_routes_fallback` for contexts where local credential-based
route inference is explicitly intended.

**Step 4: Verify authoritative routes replace placeholders**

Add a TUI test that:

1. Applies names-only `History` with `gpt-5.6-sol`.
2. Confirms the row is neutral and unavailable, not OpenAI.
3. Applies `AvailableModelsUpdated` with the authoritative Copilot route.
4. Confirms the placeholder is replaced by an available Copilot row.

**Step 5: Run focused classification tests**

```bash
cargo test -p jcode-base names_only_remote_catalog_does_not_infer_provider -- --exact --nocapture
cargo test -p jcode-tui names_only_sol_placeholder_is_replaced_by_copilot_route -- --exact --nocapture
```

Expected: PASS.

### Task 4: Make canonical commands bypass non-authoritative preview rows

**Files:**
- Modify: `crates/jcode-tui/src/tui/app/inline_interactive.rs`
- Test: `crates/jcode-tui/src/tui/app/tests/remote_model_picker_hotkeys.rs`
- Test: `crates/jcode-tui/src/tui/app/tests/remote_startup_input_01/part_01.rs`

**Step 1: Write the failing full-terminal-event test**

Build a remote app with a names-only Sol placeholder. Send every character of:

```text
/model copilot:gpt-5.6-sol
```

through `remote::handle_terminal_event`, including Enter. Assert the dummy
transport receives:

```rust
Request::SetModel {
    model: "copilot:gpt-5.6-sol".to_string(),
    ..
}
```

The test must use the full terminal dispatcher, not
`App::handle_remote_key` directly.

**Step 2: Run the test and verify it fails**

```bash
cargo test -p jcode-tui full_remote_terminal_submits_canonical_model_over_placeholder -- --exact --nocapture
```

Expected: FAIL because the fuzzy-matched unavailable row currently consumes
Enter and returns `Model unavailable`.

**Step 3: Distinguish authoritative selection from explicit submission**

When preview Enter sees a complete `/model <spec>`:

- If the active row is available and its canonical route spec exactly matches
  the typed spec, keep structured `SetRoute`.
- If the active row is unavailable, a placeholder, or canonicalizes to a
  different provider, close the preview without clearing input and continue to
  normal slash-command submission.

Do not special-case Sol or Copilot model IDs.

**Step 4: Run all command-path regressions**

```bash
cargo test -p jcode-tui full_remote_terminal_submits_canonical_model_over_placeholder -- --exact --nocapture
cargo test -p jcode-tui zero_match_model_preview -- --nocapture
cargo test -p jcode-tui remote_compact_catalog -- --nocapture
```

Expected: PASS.

### Task 5: Verify the real provider and TUI transport

**Files:**
- No additional source changes expected.

**Step 1: Run formatting and focused tests**

```bash
cargo fmt --all -- --check
git diff --check
cargo test -p jcode-app-core client_lifecycle -- --nocapture
cargo test -p jcode-app-core client_state -- --nocapture
cargo test -p jcode-base names_only_remote_catalog -- --nocapture
cargo test -p jcode-tui model_picker -- --nocapture
cargo test -p jcode-tui remote_compact_catalog -- --nocapture
```

Expected: all selected tests pass.

**Step 2: Check and build the real binary**

```bash
cargo check --bin jcode
cargo build --bin jcode
```

Expected: both commands exit successfully.

**Step 3: Run an actual Linux pseudo-terminal reproduction**

Start an isolated daemon from the freshly built binary. Drive the real TUI
through a PTY and verify:

1. `gpt-5.6-sol` appears under Copilot and never as OpenAI.
2. `/model copilot:gpt-5.6-sol` changes the server model.
3. The next prompt logs:

```text
PROVIDER_CANONICAL_INPUT: provider=copilot model=gpt-5.6-sol
```

4. The TUI receives `ModelChanged` and displays Sol as active.

**Step 4: Run a live Copilot completion**

```bash
./target/debug/jcode run -p copilot -m gpt-5.6-sol --json \
  "Reply with exactly SOL_OK"
```

Assert the JSON reports provider `Copilot`, model `gpt-5.6-sol`, and text
`SOL_OK`.

**Step 5: Inspect final repository state**

```bash
git status --short
git diff --check
git diff --stat
```

Expected: only the intended server, routing, TUI, tests, and existing plan
documents are modified. Leave every change uncommitted.
