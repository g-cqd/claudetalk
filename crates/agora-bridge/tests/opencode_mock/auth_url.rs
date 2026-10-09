use super::*;

#[test]
fn auth_from_env_reads_the_named_password_and_the_username_override() {
    let env = |name: &str| match name {
        "OC_PW" => Some("pw".to_owned()),
        "OPENCODE_SERVER_USERNAME" => Some("alice".to_owned()),
        _ => None,
    };
    assert_eq!(
        auth_from_env(Some("OC_PW"), env).expect("auth"),
        Some("Basic YWxpY2U6cHc=".to_owned())
    );
    let default_user = |name: &str| match name {
        "OC_PW" => Some("pw".to_owned()),
        _ => None,
    };
    assert_eq!(
        auth_from_env(Some("OC_PW"), default_user).expect("auth"),
        Some("Basic b3BlbmNvZGU6cHc=".to_owned())
    );
    assert_eq!(
        auth_from_env(None, |_| None).expect("no password_env"),
        None
    );
    let missing = auth_from_env(Some("OC_MISSING"), |_| None).expect_err("unset password env");
    assert!(matches!(missing, BridgeError::Client(_)), "{missing}");
}

#[test]
fn debug_output_never_shows_the_authorization_value() {
    let env = |name: &str| match name {
        "OC_PW" => Some("s3cret".to_owned()),
        _ => None,
    };
    let auth = auth_from_env(Some("OC_PW"), env).expect("auth");
    let client = OpencodeHttp::new("http://127.0.0.1:4096", auth).expect("loopback URL");
    let shown = format!("{client:?}");
    assert!(shown.contains("redacted"), "{shown}");
    assert!(
        !shown.contains("s3cret") && !shown.contains("b3BlbmNvZGU6czNjcmV0"),
        "{shown}"
    );
}

#[test]
fn url_validation_refuses_lookalike_hosts_paths_and_userinfo() {
    for url in [
        "http://127.0.0.1@evil",
        "http://localhost.attacker.com",
        "http://127.0.0.1:4096/x",
        "http://u:p@127.0.0.1",
        "http://127.0.0.1\\@evil",
        "https://127.0.0.1",
    ] {
        let err = OpencodeHttp::new(url, None).expect_err(url);
        assert!(matches!(err, BridgeError::Http(_)), "{url}: {err}");
    }
}

#[test]
fn url_validation_accepts_loopback_origins_in_any_spelling() {
    for url in [
        "http://[::1]:4096",
        "http://LOCALHOST:4096",
        "http://127.0.0.1:4096/",
    ] {
        OpencodeHttp::new(url, None).expect(url);
    }
}
