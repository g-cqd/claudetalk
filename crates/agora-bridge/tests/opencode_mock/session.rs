use super::*;

#[tokio::test]
async fn the_event_stream_carries_the_basic_auth_header() {
    let mut fake = Fake::start().await;
    let env = |name: &str| match name {
        "OC_PW" => Some("s3cret".to_owned()),
        _ => None,
    };
    let auth = auth_from_env(Some("OC_PW"), env).expect("password env is set");
    let driver = spawn_drive(OpencodeHttp::new(&fake.url, auth).expect("loopback URL"));
    fake.wait_for_stream().await;
    let events: Vec<Recorded> = fake
        .requests()
        .into_iter()
        .filter(|r| r.path == "/event")
        .collect();
    assert_eq!(events.len(), 1, "one GET /event so far");
    assert_eq!(
        events[0].auth.as_deref(),
        Some("Basic b3BlbmNvZGU6czNjcmV0")
    );
    driver.finish().await;
}

#[tokio::test]
async fn check_session_accepts_a_session_the_server_has() {
    let fake = Fake::start().await;
    let client = OpencodeHttp::new(&fake.url, None).expect("loopback URL");
    client.check_session(SESSION).await.expect("session exists");
    let reqs = fake.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].method, "GET");
    assert_eq!(reqs[0].path, "/session/ses_test");
}

#[tokio::test]
async fn an_unknown_session_given_with_session_is_a_startup_error() {
    let fake = Fake::start().await;
    let opts = OpencodeOptions {
        url: fake.url.clone(),
        session: Some("ses_missing".into()),
        ..OpencodeOptions::new(PathBuf::from("/tmp"))
    };
    let err = run(
        opts,
        PathBuf::from("/tmp/agora-unused-home"),
        CancellationToken::new(),
        None,
    )
    .await
    .expect_err("the server has no such session");
    assert!(
        matches!(&err, BridgeError::Http(m) if m == "opencode session ses_missing not found"),
        "{err}"
    );
    let reqs = fake.requests();
    assert_eq!(reqs.len(), 1, "the check is the only request");
    assert_eq!(reqs[0].method, "GET");
    assert_eq!(reqs[0].path, "/session/ses_missing");
}

#[tokio::test]
async fn a_session_that_never_goes_idle_gets_its_prompt_after_the_busy_bound() {
    let mut fake = Fake::start().await;
    let client = OpencodeHttp::new(&fake.url, None).expect("loopback URL");
    let driver = spawn_drive_bounded(client, SHORT_BUSY);
    fake.wait_for_stream().await;
    fake.push(status_frame(SESSION, "busy").as_bytes());
    tokio::time::sleep(SETTLE).await;

    let mut answer = driver.deliver("stuck").await;
    assert!(
        timeout(QUIET, &mut answer).await.is_err(),
        "the delivery went out before the busy bound"
    );
    assert!(
        answered(answer).await,
        "the prompt goes out once the bound passes"
    );
    assert_eq!(fake.prompts().len(), 1);
    driver.finish().await;
}

#[tokio::test]
async fn shutdown_ends_the_driver_while_the_event_stream_never_answers() {
    // Accepts connections and never answers, so `GET /event` hangs before its headers.
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let url = format!("http://{}", listener.local_addr().expect("local addr"));
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
        }
    });
    let driver = spawn_drive(OpencodeHttp::new(&url, None).expect("loopback URL"));
    tokio::time::sleep(SETTLE).await;
    driver.finish().await;
}
