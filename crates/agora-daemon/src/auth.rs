//! Handshake and peer-credential checks (pure helpers; server.rs drives the I/O).

use agora_proto::{ClientHello, ErrorBody, ErrorCode};
use agora_store::Pub;

/// Longest client label kept, in bytes.
const MAX_LABEL_BYTES: usize = 64;

/// 32 bytes from `agora_crypto::random_bytes::<32>()`. Panics (expect) only if the
/// OS RNG is unavailable, which is unrecoverable.
pub fn new_challenge() -> [u8; 32] {
    agora_crypto::random_bytes::<32>().expect("OS RNG unavailable")
}

/// Verify a ClientHello against the challenge we sent. Any failure ->
/// ErrorBody{Unauthenticated, "handshake failed: <reason>"}.
pub fn verify_hello(challenge: &[u8; 32], hello: &ClientHello) -> Result<Pub, ErrorBody> {
    hello
        .verify(challenge)
        .map_err(|e| ErrorBody::new(ErrorCode::Unauthenticated, format!("handshake failed: {e}")))
}

/// Effective uid of this process (libc::geteuid).
pub fn daemon_uid() -> u32 {
    // SAFETY: geteuid(2) takes no arguments, has no preconditions, and cannot fail.
    unsafe { libc::geteuid() }
}

/// uid of the process at the other end of a unix stream (tokio `peer_cred`).
pub fn peer_uid(stream: &tokio::net::UnixStream) -> std::io::Result<u32> {
    Ok(stream.peer_cred()?.uid())
}

/// `label` cut to at most 64 bytes on a char boundary, control chars removed.
pub fn clean_label(label: &str) -> String {
    let mut out = String::with_capacity(label.len().min(MAX_LABEL_BYTES));
    for c in label.chars().filter(|c| !c.is_control()) {
        if out.len() + c.len_utf8() > MAX_LABEL_BYTES {
            break;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use agora_crypto::Identity;

    #[test]
    fn two_challenges_differ() {
        assert_ne!(new_challenge(), new_challenge());
    }

    #[test]
    fn clean_label_truncates_multibyte_on_char_boundary() {
        let cleaned = clean_label(&"é".repeat(40));
        assert_eq!(cleaned.len(), 64);
        assert_eq!(cleaned.chars().count(), 32);

        // 'a' + 40 × 'é' is 81 bytes; the 32nd 'é' would end at byte 65, so the
        // result stops at 63 bytes instead of splitting a character.
        let cleaned = clean_label(&format!("a{}", "é".repeat(40)));
        assert_eq!(cleaned.len(), 63);
        assert!(cleaned.chars().all(|c| c == 'a' || c == 'é'));
    }

    #[test]
    fn clean_label_removes_control_chars() {
        assert_eq!(clean_label("a\tb\nc\u{7f}d\u{0}e"), "abcde");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn peer_uid_of_socketpair_is_daemon_uid() {
        let (a, _b) = tokio::net::UnixStream::pair().expect("socketpair");
        assert_eq!(peer_uid(&a).expect("peer_cred"), daemon_uid());
    }

    #[test]
    fn verify_hello_accepts_hello_signed_for_this_challenge() {
        let id = Identity::from_seed([5; 32]);
        let challenge = new_challenge();
        let hello = ClientHello::sign(&id, &challenge, "x");
        assert_eq!(
            verify_hello(&challenge, &hello).expect("valid hello"),
            id.public()
        );
    }

    #[test]
    fn verify_hello_rejects_hello_signed_for_another_challenge() {
        let id = Identity::from_seed([5; 32]);
        let challenge = new_challenge();
        let other = new_challenge();
        let hello = ClientHello::sign(&id, &other, "x");
        let err = verify_hello(&challenge, &hello).expect_err("stale hello must fail");
        assert_eq!(err.code, ErrorCode::Unauthenticated);
    }
}
