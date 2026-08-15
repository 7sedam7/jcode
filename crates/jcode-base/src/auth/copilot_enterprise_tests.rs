use super::*;

#[test]
fn dotcom_urls_match_the_public_endpoints() {
    let deployment = CopilotDeployment::DotCom;
    assert_eq!(
        deployment.device_code_url(),
        "https://github.com/login/device/code"
    );
    assert_eq!(
        deployment.access_token_url(),
        "https://github.com/login/oauth/access_token"
    );
    assert_eq!(deployment.api_base(), "https://api.githubcopilot.com");
    assert_eq!(deployment.rest_api_base(), "https://api.github.com");
    assert!(!deployment.is_enterprise());
}

#[test]
fn enterprise_urls_are_derived_from_the_domain() {
    // Mirrors OpenCode: device flow at the enterprise host, Copilot API at
    // copilot-api.<domain>.
    let deployment = CopilotDeployment::from_domain_input("company.ghe.com").unwrap();
    assert_eq!(
        deployment.device_code_url(),
        "https://company.ghe.com/login/device/code"
    );
    assert_eq!(
        deployment.access_token_url(),
        "https://company.ghe.com/login/oauth/access_token"
    );
    assert_eq!(deployment.api_base(), "https://copilot-api.company.ghe.com");
    assert_eq!(deployment.rest_api_base(), "https://api.company.ghe.com");
    assert_eq!(deployment.host(), "company.ghe.com");
    assert!(deployment.is_enterprise());
}

#[test]
fn domain_input_accepts_the_forms_users_paste() {
    for input in [
        "company.ghe.com",
        "https://company.ghe.com",
        "http://company.ghe.com",
        "https://company.ghe.com/",
        "  https://Company.GHE.com/  ",
        "https://company.ghe.com/some/path",
    ] {
        assert_eq!(
            normalize_domain(input).unwrap(),
            "company.ghe.com",
            "input: {input:?}"
        );
    }
}

#[test]
fn domain_input_rejects_nonsense() {
    for input in ["", "   ", "not a domain", "localhost", "user@company.com"] {
        assert!(normalize_domain(input).is_err(), "input: {input:?}");
    }
}

#[test]
fn a_port_is_rejected_because_it_cannot_form_a_copilot_api_host() {
    // `copilot-api.company.ghe.com:8443` is not a hostname, so accepting this
    // would produce requests to a URL that cannot resolve.
    assert!(normalize_domain("company.ghe.com:8443").is_err());
}

#[test]
fn naming_github_com_is_not_an_enterprise_deployment() {
    // Otherwise jcode would build `copilot-api.github.com`, which does not exist.
    for input in ["github.com", "https://github.com", "api.github.com"] {
        assert_eq!(
            CopilotDeployment::from_domain_input(input).unwrap(),
            CopilotDeployment::DotCom,
            "input: {input:?}"
        );
    }
}

#[test]
fn env_override_selects_the_enterprise_deployment() {
    let _sandbox = crate::auth::test_sandbox::AuthTestSandbox::new().unwrap();
    crate::env::set_var(COPILOT_ENTERPRISE_URL_ENV, "https://company.ghe.com/");
    assert_eq!(
        current_deployment(),
        CopilotDeployment::Enterprise("company.ghe.com".to_string())
    );
}

#[test]
fn a_blank_env_override_means_dotcom_even_with_a_persisted_domain() {
    let _sandbox = crate::auth::test_sandbox::AuthTestSandbox::new().unwrap();
    save_deployment(&CopilotDeployment::Enterprise("company.ghe.com".into())).unwrap();
    crate::env::set_var(COPILOT_ENTERPRISE_URL_ENV, "  ");
    assert_eq!(current_deployment(), CopilotDeployment::DotCom);
}

#[test]
fn a_saved_deployment_survives_into_the_next_session() {
    let _sandbox = crate::auth::test_sandbox::AuthTestSandbox::new().unwrap();

    assert_eq!(current_deployment(), CopilotDeployment::DotCom);
    let enterprise = CopilotDeployment::Enterprise("company.ghe.com".to_string());
    save_deployment(&enterprise).unwrap();
    assert_eq!(current_deployment(), enterprise);

    // Logging back in on dotcom must clear the enterprise domain, not leave a
    // stale one that would send dotcom tokens to the enterprise endpoint.
    save_deployment(&CopilotDeployment::DotCom).unwrap();
    assert_eq!(current_deployment(), CopilotDeployment::DotCom);
}

#[test]
fn an_unreadable_deployment_file_falls_back_to_dotcom() {
    let _sandbox = crate::auth::test_sandbox::AuthTestSandbox::new().unwrap();
    let path = deployment_path();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "{ not json").unwrap();
    assert_eq!(current_deployment(), CopilotDeployment::DotCom);
}

/// A real `/copilot_internal/user` response from an enterprise seat held on a
/// personal github.com account. The endpoint is *not* api.githubcopilot.com,
/// which no amount of string-building from "github.com" would produce.
const REAL_ENTERPRISE_SEAT: &str = r#"{
  "login": "someone",
  "access_type_sku": "copilot_enterprise_seat_quota",
  "copilot_plan": "enterprise",
  "chat_enabled": true,
  "organization_login_list": ["ExampleCorp"],
  "endpoints": {
    "api": "https://api.enterprise.githubcopilot.com",
    "proxy": "https://proxy.enterprise.githubcopilot.com"
  }
}"#;

#[test]
fn an_enterprise_seat_reports_its_own_api_endpoint() {
    let info: CopilotUserInfo = serde_json::from_str(REAL_ENTERPRISE_SEAT).unwrap();
    assert_eq!(info.copilot_plan, "enterprise");
    assert_eq!(
        info.api_base(),
        Some("https://api.enterprise.githubcopilot.com")
    );
    assert_eq!(
        info.account_type(),
        crate::auth::copilot::CopilotAccountType::Enterprise
    );
    assert_eq!(
        info.organization_login_list,
        vec!["ExampleCorp".to_string()]
    );
}

#[test]
fn a_seat_without_endpoints_falls_back_to_the_deployment_default() {
    let info: CopilotUserInfo =
        serde_json::from_str(r#"{"login":"someone","copilot_plan":"individual"}"#).unwrap();
    assert_eq!(info.api_base(), None);
    assert_eq!(
        info.account_type(),
        crate::auth::copilot::CopilotAccountType::Individual
    );
}

#[test]
fn a_non_https_endpoint_is_ignored() {
    // Never downgrade to plaintext on the say-so of a response body.
    let info: CopilotUserInfo =
        serde_json::from_str(r#"{"endpoints":{"api":"http://evil.example"}}"#).unwrap();
    assert_eq!(info.api_base(), None);
}

#[test]
fn the_discovered_endpoint_wins_over_the_constructed_one() {
    let _sandbox = crate::auth::test_sandbox::AuthTestSandbox::new().unwrap();
    clear_discovered_api_base();

    // Before discovery: the dotcom default.
    assert_eq!(api_base(), "https://api.githubcopilot.com");

    // After: whatever GitHub said, which is the whole point.
    record_discovered_api_base("https://api.enterprise.githubcopilot.com/");
    assert_eq!(api_base(), "https://api.enterprise.githubcopilot.com");

    clear_discovered_api_base();
    assert_eq!(api_base(), "https://api.githubcopilot.com");
}

#[test]
fn discovery_also_wins_over_an_explicit_enterprise_domain() {
    // A GHES tenant that reports an endpoint knows better than the
    // `copilot-api.<domain>` convention.
    let _sandbox = crate::auth::test_sandbox::AuthTestSandbox::new().unwrap();
    clear_discovered_api_base();
    crate::env::set_var(COPILOT_ENTERPRISE_URL_ENV, "company.ghe.com");

    assert_eq!(api_base(), "https://copilot-api.company.ghe.com");
    record_discovered_api_base("https://copilot.company.ghe.com");
    assert_eq!(api_base(), "https://copilot.company.ghe.com");

    clear_discovered_api_base();
}
