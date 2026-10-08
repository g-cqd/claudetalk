//! Connection protocol, registration policy, and socket lifecycle: what a peer sees
//! before and around the handshake, and what the daemon does with its home directory.

mod common;

use agora_crypto::Identity;
use agora_daemon::{DaemonError, Paths, now_ms};
use agora_proto::{
    ClientHello, ErrorCode, MAX_FRAME, MessageWire, Reply, Req, ServerFrame, read_json, unb64,
    write_frame, write_json,
};
use agora_store::{SignedMessage, hex};
use common::*;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;

/// Longest any single read may take; a daemon bug fails the test instead of hanging it.
const READ: Duration = Duration::from_secs(5);

/// Next server frame within `limit`; `None` on a clean EOF. Panics on silence or a read error.
async fn next_frame_within(stream: &mut UnixStream, limit: Duration) -> Option<ServerFrame> {
    match tokio::time::timeout(limit, read_json::<ServerFrame, _>(stream)).await {
        Err(_) => panic!("no frame within {limit:?}"),
        Ok(Err(e)) => panic!("reading a frame failed: {e}"),
        Ok(Ok(frame)) => frame,
    }
}

async fn next_frame(stream: &mut UnixStream) -> Option<ServerFrame> {
    next_frame_within(stream, READ).await
}

/// Reads the server's Hello on a fresh connection and returns the decoded challenge.
async fn read_hello(stream: &mut UnixStream) -> Vec<u8> {
    match next_frame(stream).await {
        Some(ServerFrame::Hello(hello)) => {
            unb64(&hello.challenge, "challenge").expect("challenge is base64")
        }
        other => panic!("expected Hello first, got {other:?}"),
    }
}

/// A Post of a plaintext message to `channel`, signed by `id` (the connection's own key).
fn post_req(id: &Identity, channel: &str) -> Req {
    let msg = SignedMessage {
        id: "m-outsider".to_string(),
        channel: channel.to_string(),
        thread_of: None,
        sender: [0; 32],
        ts: now_ms(),
        encrypted: false,
        epoch: 0,
        body: b"hi".to_vec(),
        attachments: vec![],
        sig: vec![],
    }
    .sign(id);
    Req::Post {
        msg: MessageWire::from_message(&msg),
    }
}

/// A daemon bound on a home the caller created, wrapped so it stops like `start()`'s.
async fn start_on(
    home: tempfile::TempDir,
    f: impl FnOnce(&mut agora_daemon::DaemonConfig),
) -> TestDaemon {
    let socket = Paths::new(home.path()).socket;
    let (token, task) = start_at(home.path(), f)
        .await
        .expect("bind daemon on the given home");
    TestDaemon {
        home,
        socket,
        token,
        task,
    }
}

fn temp_home() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("ag")
        .tempdir_in("/tmp")
        .expect("create temp home under /tmp")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hello_is_the_first_frame_and_offers_version_one_with_a_32_byte_challenge() {
    let d = start().await;
    let mut stream = raw_connect(&d).await;

    match next_frame(&mut stream).await {
        Some(ServerFrame::Hello(hello)) => {
            assert_eq!(hello.version, 1);
            let challenge = unb64(&hello.challenge, "challenge").expect("challenge is base64");
            assert_eq!(challenge.len(), 32);
        }
        other => panic!("expected Hello as the first frame, got {other:?}"),
    }

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hello_signed_over_another_challenge_is_unauthenticated_and_the_connection_closes() {
    let d = start().await;
    let mut stream = raw_connect(&d).await;
    let challenge = read_hello(&mut stream).await;

    let mut stale = challenge.clone();
    stale[0] ^= 0xff;
    write_json(&mut stream, &ClientHello::sign(&ident(1), &stale, "test"))
        .await
        .expect("send ClientHello");

    match next_frame(&mut stream).await {
        Some(ServerFrame::Err { id: 0, error }) => {
            assert_eq!(error.code, ErrorCode::Unauthenticated);
        }
        other => panic!("expected Unauthenticated on id 0, got {other:?}"),
    }
    assert!(
        next_frame(&mut stream).await.is_none(),
        "daemon closes the connection after a failed handshake"
    );

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hello_naming_another_identitys_pubkey_is_unauthenticated() {
    let d = start().await;
    let mut stream = raw_connect(&d).await;
    let challenge = read_hello(&mut stream).await;

    // Signed by key A, but the pubkey field claims key B.
    let mut hello = ClientHello::sign(&ident(1), &challenge, "test");
    hello.pubkey = hex(&ident(2).public());
    write_json(&mut stream, &hello)
        .await
        .expect("send ClientHello");

    match next_frame(&mut stream).await {
        Some(ServerFrame::Err { id: 0, error }) => {
            assert_eq!(error.code, ErrorCode::Unauthenticated);
        }
        other => panic!("expected Unauthenticated on id 0, got {other:?}"),
    }
    assert!(next_frame(&mut stream).await.is_none(), "connection closed");

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn malformed_json_as_hello_is_unauthenticated() {
    let d = start().await;
    let mut stream = raw_connect(&d).await;
    let _challenge = read_hello(&mut stream).await;

    write_frame(&mut stream, b"{not a hello")
        .await
        .expect("send garbage frame");

    match next_frame(&mut stream).await {
        Some(ServerFrame::Err { id: 0, error }) => {
            assert_eq!(error.code, ErrorCode::Unauthenticated);
        }
        other => panic!("expected Unauthenticated on id 0, got {other:?}"),
    }

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn silent_client_is_unauthenticated_within_the_handshake_timeout() {
    let d = start_with(|c| c.handshake_timeout_ms = 200).await;
    let mut stream = raw_connect(&d).await;
    let _challenge = read_hello(&mut stream).await;

    match next_frame_within(&mut stream, Duration::from_secs(2)).await {
        Some(ServerFrame::Err { id: 0, error }) => {
            assert_eq!(error.code, ErrorCode::Unauthenticated);
        }
        other => panic!("expected Unauthenticated on id 0 after the timeout, got {other:?}"),
    }

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn welcome_names_the_proven_key_and_its_pseudonym() {
    let d = start().await;
    let id = ident(14);
    let mut stream = raw_connect(&d).await;
    let challenge = read_hello(&mut stream).await;
    write_json(&mut stream, &ClientHello::sign(&id, &challenge, "test"))
        .await
        .expect("send ClientHello");

    match next_frame(&mut stream).await {
        Some(ServerFrame::Welcome(welcome)) => {
            assert_eq!(welcome.pubkey, hex(&id.public()));
            assert_eq!(welcome.pseudonym, agora_crypto::pseudonym(&id.public()));
        }
        other => panic!("expected Welcome, got {other:?}"),
    }

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unregistered_key_sees_no_role_and_no_owner_in_whoami() {
    let d = start().await;
    let mut c = connect(&d, ident(5)).await;

    let reply = c.ok(Req::Whoami).await;
    assert_eq!(
        reply,
        Reply::Whoami {
            pubkey: c.pk_hex(),
            pseudonym: agora_crypto::pseudonym(&c.id.public()),
            role: None,
            registered: false,
            revoked: false,
            owner_exists: false,
        }
    );

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bootstrap_owner_makes_the_connection_key_the_owner() {
    let d = start().await;
    let mut owner = connect(&d, ident(1)).await;

    assert_eq!(owner.ok(Req::BootstrapOwner).await, Reply::Unit);

    let reply = owner.ok(Req::Whoami).await;
    assert_eq!(
        reply,
        Reply::Whoami {
            pubkey: owner.pk_hex(),
            pseudonym: agora_crypto::pseudonym(&owner.id.public()),
            role: Some("owner".to_string()),
            registered: true,
            revoked: false,
            owner_exists: true,
        }
    );

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_owner_bootstrap_is_invalid() {
    let d = start().await;
    let mut first = connect(&d, ident(1)).await;
    assert_eq!(first.ok(Req::BootstrapOwner).await, Reply::Unit);

    let mut second = connect(&d, ident(2)).await;
    match second.call(Req::BootstrapOwner).await {
        Err(error) => assert_eq!(error.code, ErrorCode::Invalid),
        Ok(reply) => panic!("second owner bootstrap must fail, got {reply:?}"),
    }

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn closed_registration_refuses_register_but_still_bootstraps_the_owner() {
    let d = start_with(|c| c.open_registration = false).await;

    let mut newcomer = connect(&d, ident(3)).await;
    match newcomer
        .call(Req::Register {
            label: "alice".to_string(),
        })
        .await
    {
        Err(error) => assert_eq!(error.code, ErrorCode::RegistrationClosed),
        Ok(reply) => panic!("Register must be refused while closed, got {reply:?}"),
    }

    let mut owner = connect(&d, ident(1)).await;
    assert_eq!(owner.ok(Req::BootstrapOwner).await, Reply::Unit);

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn registering_twice_leaves_the_same_state_as_once() {
    let d = start().await;
    let mut c = connect(&d, ident(3)).await;
    let register = Req::Register {
        label: "alice".to_string(),
    };

    assert_eq!(c.ok(register.clone()).await, Reply::Unit);
    let once = c.ok(Req::Whoami).await;

    assert_eq!(c.ok(register).await, Reply::Unit);
    let twice = c.ok(Req::Whoami).await;

    assert_eq!(twice, once);
    match twice {
        Reply::Whoami {
            registered: true,
            revoked: false,
            ..
        } => {}
        other => panic!("expected a registered, unrevoked identity, got {other:?}"),
    }

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unregistered_key_is_forbidden_to_post_or_read() {
    let d = start().await;
    let mut w = world(&d).await;
    w.owner.create_group("general", &[&w.alice, &w.bob]).await;

    let mut outsider = connect(&d, ident(9)).await;

    let post = post_req(&outsider.id, "general");
    match outsider.call(post).await {
        Err(error) => assert_eq!(error.code, ErrorCode::Forbidden),
        Ok(reply) => panic!("unregistered Post must be forbidden, got {reply:?}"),
    }

    let read = Req::Read {
        channel: "general".to_string(),
        after: 0,
        limit: 10,
    };
    match outsider.call(read).await {
        Err(error) => assert_eq!(error.code, ErrorCode::Forbidden),
        Ok(reply) => panic!("unregistered Read must be forbidden, got {reply:?}"),
    }

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oversized_frame_header_closes_that_connection_and_the_daemon_keeps_serving() {
    let d = start().await;
    let mut c = connect(&d, ident(8)).await;

    // Only the length header: the daemon must refuse it without reading a payload.
    let declared = u32::try_from(MAX_FRAME + 1).expect("MAX_FRAME + 1 fits in u32");
    c.stream
        .write_all(&declared.to_be_bytes())
        .await
        .expect("send oversized header");

    match next_frame(&mut c.stream).await {
        Some(ServerFrame::Err { id: 0, error }) => assert_eq!(error.code, ErrorCode::TooLarge),
        other => panic!("expected TooLarge on id 0, got {other:?}"),
    }
    assert!(
        next_frame(&mut c.stream).await.is_none(),
        "the stream cannot be resynchronised, so the daemon closes it"
    );

    let mut fresh = connect(&d, ident(10)).await;
    assert_eq!(fresh.ok(Req::Ping).await, Reply::Pong);

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn malformed_json_after_the_handshake_is_bad_request_and_the_connection_keeps_serving() {
    let d = start().await;
    let mut c = connect(&d, ident(13)).await;

    write_frame(&mut c.stream, b"{not json")
        .await
        .expect("send garbage frame");
    match next_frame(&mut c.stream).await {
        Some(ServerFrame::Err { id: 0, error }) => assert_eq!(error.code, ErrorCode::BadRequest),
        other => panic!("expected BadRequest on id 0, got {other:?}"),
    }

    assert_eq!(c.ok(Req::Ping).await, Reply::Pong);

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_operation_is_bad_request_answered_under_the_request_id() {
    let d = start().await;
    let mut c = connect(&d, ident(15)).await;

    write_frame(&mut c.stream, br#"{"id":42,"req":"no_such_op"}"#)
        .await
        .expect("send unknown operation");
    match next_frame(&mut c.stream).await {
        Some(ServerFrame::Err { id: 42, error }) => assert_eq!(error.code, ErrorCode::BadRequest),
        other => panic!("expected BadRequest on id 42, got {other:?}"),
    }

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_socket_file_from_a_dead_daemon_is_replaced_and_the_daemon_serves() {
    let home = temp_home();
    let socket = Paths::new(home.path()).socket;
    // Bind and drop: the socket file stays behind, as after a crash.
    let listener = UnixListener::bind(&socket).expect("plant a stale socket");
    drop(listener);
    assert!(
        std::fs::symlink_metadata(&socket)
            .expect("stale socket exists")
            .file_type()
            .is_socket(),
        "precondition: a socket file was left behind"
    );

    let d = start_on(home, |_| {}).await;
    let mut c = connect(&d, ident(11)).await;
    assert_eq!(c.ok(Req::Ping).await, Reply::Pong);

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn second_daemon_on_a_live_home_is_already_running_and_the_first_keeps_serving() {
    let d = start().await;

    let err = match start_at(d.home.path(), |_| {}).await {
        Ok(_) => panic!("a second daemon must not bind a home a live daemon holds"),
        Err(err) => err,
    };
    assert!(matches!(err, DaemonError::AlreadyRunning(_)), "got {err:?}");

    let mut c = connect(&d, ident(16)).await;
    assert_eq!(c.ok(Req::Ping).await, Reply::Pong);

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn home_directory_is_made_private_to_its_owner() {
    let home = temp_home();
    // Start loose so the daemon has to tighten it.
    std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o755))
        .expect("loosen the home first");

    let d = start_on(home, |_| {}).await;
    let mode = std::fs::metadata(d.home.path())
        .expect("home exists")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o700);

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn socket_file_is_owner_read_write_only() {
    let d = start().await;

    let mode = std::fs::metadata(&d.socket)
        .expect("socket exists while running")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn socket_file_is_removed_when_the_daemon_stops() {
    let d = start().await;
    let socket = d.socket.clone();
    assert!(socket.exists(), "precondition: the daemon is listening");

    d.stop().await;

    assert!(!socket.exists(), "the socket file is removed on shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_request_is_answered_unit_and_the_daemon_task_finishes_within_five_seconds() {
    let d = start().await;
    let mut c = connect(&d, ident(12)).await;

    assert_eq!(c.ok(Req::Shutdown).await, Reply::Unit);

    match tokio::time::timeout(Duration::from_secs(5), d.task).await {
        Err(_) => panic!("daemon did not finish within 5 s of a Shutdown request"),
        Ok(joined) => {
            let run = joined.expect("daemon run task joins");
            run.expect("daemon stops cleanly after Shutdown");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revoked_key_cannot_shut_the_daemon_down_and_the_daemon_keeps_serving() {
    let d = start().await;
    let mut w = world(&d).await;
    let revoke = w.owner.op("identity.revoke", &w.bob.pk_hex(), "");
    assert_eq!(w.owner.ok(Req::Revoke { op: revoke }).await, Reply::Unit);

    match w.bob.call(Req::Shutdown).await {
        Err(error) => assert_eq!(error.code, ErrorCode::Forbidden),
        Ok(reply) => panic!("a revoked key's Shutdown must be forbidden, got {reply:?}"),
    }

    assert_eq!(w.carol.ok(Req::Ping).await, Reply::Pong);

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn closed_registration_answers_unit_for_a_registered_key_and_refuses_an_unknown_one() {
    let d = start_with(|c| c.open_registration = false).await;
    let mut owner = connect(&d, ident(1)).await;
    assert_eq!(owner.ok(Req::BootstrapOwner).await, Reply::Unit);

    assert_eq!(
        owner
            .ok(Req::Register {
                label: "owner".to_string(),
            })
            .await,
        Reply::Unit,
        "registering an already-registered key is idempotent even when closed"
    );

    let mut newcomer = connect(&d, ident(3)).await;
    match newcomer
        .call(Req::Register {
            label: "alice".to_string(),
        })
        .await
    {
        Err(error) => assert_eq!(error.code, ErrorCode::RegistrationClosed),
        Ok(reply) => {
            panic!("an unknown key's Register must be refused while closed, got {reply:?}")
        }
    }

    d.stop().await;
}
