use super::*;

#[tokio::test]
async fn create_session_posts_the_title_and_returns_the_id() {
    let fake = Fake::start().await;
    let client = OpencodeHttp::new(&fake.url, None).expect("loopback URL");
    assert_eq!(client.create_session().await.expect("session"), SESSION);
    let reqs = fake.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].method, "POST");
    assert_eq!(reqs[0].path, "/session");
    assert_eq!(reqs[0].body, json!({"title": "agora"}));
    assert_eq!(
        reqs[0].auth, None,
        "no password_env, so no Authorization header"
    );
}

#[tokio::test]
async fn prompt_async_posts_the_text_part_with_basic_auth_from_the_password_env() {
    let fake = Fake::start().await;
    let env = |name: &str| match name {
        "OC_PW" => Some("s3cret".to_owned()),
        _ => None,
    };
    let auth = auth_from_env(Some("OC_PW"), env).expect("password env is set");
    let client = OpencodeHttp::new(&fake.url, auth).expect("loopback URL");
    client
        .prompt_async(SESSION, "hello")
        .await
        .expect("2xx is accepted");
    let prompts = fake.prompts();
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0].path, "/session/ses_test/prompt_async");
    assert_eq!(
        prompts[0].body,
        json!({"parts": [{"type": "text", "text": "hello"}]})
    );
    assert_eq!(
        prompts[0].auth.as_deref(),
        Some("Basic b3BlbmNvZGU6czNjcmV0")
    );
}

#[tokio::test]
async fn a_delivery_waits_while_the_server_reports_busy_and_goes_out_on_idle() {
    let mut fake = Fake::start().await;
    let driver = spawn_drive(OpencodeHttp::new(&fake.url, None).expect("loopback URL"));
    fake.wait_for_stream().await;
    // The frame arrives in two chunks: the parser must join them.
    let frame = status_frame(SESSION, "busy");
    let (head, tail) = frame.as_bytes().split_at(12);
    fake.push(head);
    fake.push(tail);
    tokio::time::sleep(SETTLE).await;

    let mut answer = driver.deliver("first").await;
    assert!(
        timeout(QUIET, &mut answer).await.is_err(),
        "a delivery was answered while the session was busy"
    );
    assert!(fake.prompts().is_empty(), "no prompt_async while busy");

    fake.push(idle_frame(SESSION).as_bytes());
    assert!(answered(answer).await, "accepted once the session is idle");
    assert_eq!(fake.prompts().len(), 1);
    driver.finish().await;
}

#[tokio::test]
async fn a_prompt_the_server_refuses_replies_false() {
    let fake = Fake::start().await;
    fake.set_prompt_status(500);
    let driver = spawn_drive(OpencodeHttp::new(&fake.url, None).expect("loopback URL"));
    let answer = driver.deliver("refused").await;
    assert!(!answered(answer).await, "a non-2xx answer is a refusal");
    assert_eq!(fake.prompts().len(), 1, "the prompt was attempted once");
    driver.finish().await;
}

#[tokio::test]
async fn a_non_loopback_url_is_refused_before_any_request() {
    for url in [
        "http://192.168.1.10:4096",
        "http://example.com:4096",
        "https://127.0.0.1:4096",
        "http://127.0.0.2:4096",
    ] {
        let err = OpencodeHttp::new(url, None).expect_err(url);
        assert!(
            matches!(&err, BridgeError::Http(m) if m == "opencode bridge only talks to a loopback server"),
            "{url}: {err}"
        );
    }
    let opts = OpencodeOptions {
        url: "http://example.com:4096".into(),
        ..OpencodeOptions::new(PathBuf::from("/tmp"))
    };
    let err = run(
        opts,
        PathBuf::from("/tmp/agora-unused-home"),
        CancellationToken::new(),
        None,
    )
    .await
    .expect_err("run refuses a non-loopback URL");
    assert!(matches!(err, BridgeError::Http(_)), "{err}");
    for url in [
        "http://127.0.0.1:4096",
        "http://localhost:4096",
        "http://[::1]:4096",
    ] {
        OpencodeHttp::new(url, None).expect(url);
    }
}

#[tokio::test]
async fn events_for_other_sessions_do_not_change_our_busy_state() {
    let mut fake = Fake::start().await;
    let driver = spawn_drive(OpencodeHttp::new(&fake.url, None).expect("loopback URL"));
    fake.wait_for_stream().await;

    // Another session's busy status does not hold our delivery.
    fake.push(status_frame("ses_other", "busy").as_bytes());
    tokio::time::sleep(SETTLE).await;
    assert!(
        answered(driver.deliver("first").await).await,
        "another session's busy status blocked our delivery"
    );

    // Our own prompt left us busy; another session's idle must not free us.
    fake.push(idle_frame("ses_other").as_bytes());
    tokio::time::sleep(SETTLE).await;
    let mut second = driver.deliver("second").await;
    assert!(
        timeout(QUIET, &mut second).await.is_err(),
        "another session's idle freed our session"
    );
    assert_eq!(fake.prompts().len(), 1);

    fake.push(idle_frame(SESSION).as_bytes());
    assert!(answered(second).await, "our own idle frees the session");
    assert_eq!(fake.prompts().len(), 2);
    driver.finish().await;
}
