use super::*;
use anyhow::Result;

#[test]
fn journal_roundtrip_preserves_changed_route_api_method() -> Result<()> {
    let _env_lock = lock_env();
    let temp_home = tempfile::tempdir()?;
    let _home = EnvVarGuard::set("JCODE_HOME", temp_home.path());
    let session_id = "session_journal_route_roundtrip";
    let mut session = Session::create_with_id(session_id.to_string(), None, None);
    session.add_message(
        Role::User,
        vec![ContentBlock::Text {
            text: "first".to_string(),
            cache_control: None,
        }],
    );
    session.save()?;

    session.provider_key = Some("copilot".to_string());
    session.route_api_method = Some("copilot".to_string());
    session.model = Some("gpt-5.6-terra".to_string());
    session.add_message(
        Role::Assistant,
        vec![ContentBlock::Text {
            text: "second".to_string(),
            cache_control: None,
        }],
    );
    session.save()?;

    let restored = Session::load(session_id)?;
    assert_eq!(restored.provider_key.as_deref(), Some("copilot"));
    assert_eq!(restored.route_api_method.as_deref(), Some("copilot"));
    assert_eq!(restored.model.as_deref(), Some("gpt-5.6-terra"));
    Ok(())
}
