//! Opt-in live check that jcode discovers the seat's own API endpoint from
//! GitHub rather than assuming `api.githubcopilot.com`.
//!
//! Ignored by default; needs a GitHub OAuth token in `COPILOT_LIVE_TOKEN`.

use jcode_base::auth::copilot_enterprise::{api_base, fetch_user_info, record_discovered_api_base};

#[tokio::test]
#[ignore = "hits the live GitHub API"]
async fn the_seat_lookup_reports_a_plan_and_an_endpoint() {
    let token = std::env::var("COPILOT_LIVE_TOKEN").expect("COPILOT_LIVE_TOKEN");
    let client = reqwest::Client::new();

    let info = fetch_user_info(&client, &token)
        .await
        .expect("seat lookup succeeds");

    assert!(
        !info.copilot_plan.is_empty(),
        "GitHub should name the plan, got {info:?}"
    );

    // Whatever endpoint GitHub names must be usable, and must be what jcode
    // then targets. This is the assertion that would have caught jcode
    // hardcoding the wrong base for an enterprise seat.
    if let Some(discovered) = info.api_base() {
        assert!(discovered.starts_with("https://"), "{discovered}");
        record_discovered_api_base(discovered);
        assert_eq!(api_base(), discovered.trim_end_matches('/'));

        let status = client
            .get(format!("{}/models", api_base()))
            .bearer_auth(&token)
            .header("X-GitHub-Api-Version", "2026-06-01")
            .send()
            .await
            .expect("catalog request completes")
            .status();
        assert!(status.is_success(), "discovered endpoint returned {status}");
    }
}
