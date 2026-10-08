//! Request routing and the error mapping shared by every handler module.

use crate::{Conn, DaemonError, Shared};
use agora_core::TransitionError;
use agora_proto::{ErrorBody, ErrorCode, OpWire, ProtoError, Reply, Req};
use agora_store::{Pub, SignedOp, StoreError};

/// Route one request. Ping is answered here; every other variant goes to exactly
/// one module. (Routing table is fixed; implement the helpers below.)
pub async fn handle(sh: &Shared, conn: &Conn, req: Req) -> Result<Reply, ErrorBody> {
    match req {
        Req::Ping => Ok(Reply::Pong),
        Req::InboxWait {
            after,
            timeout_ms,
            limit,
        } => crate::wait::inbox_wait(sh, conn, after, timeout_ms, limit).await,
        Req::Subscribe { .. } => Err(ErrorBody::new(
            ErrorCode::Invalid,
            "subscribe is handled by the connection",
        )),
        Req::Online => crate::push::online(sh, conn).await,
        r @ (Req::PutBlob { .. }
        | Req::GetBlob { .. }
        | Req::Post { .. }
        | Req::Read { .. }
        | Req::Inbox { .. }
        | Req::Ack { .. }
        | Req::React { .. }
        | Req::ReactionCounts { .. }) => crate::chat::handle(sh, conn, r).await,
        r @ (Req::CardCreate { .. }
        | Req::CardMove { .. }
        | Req::CardEvidence { .. }
        | Req::CardApprove { .. }
        | Req::CardReclaim { .. }
        | Req::CardList { .. }
        | Req::SprintCreate { .. }
        | Req::SprintAdd { .. }
        | Req::SprintStart { .. }
        | Req::SprintClose { .. }
        | Req::SprintSummary { .. }
        | Req::NotePublish { .. }
        | Req::NoteSearch { .. }) => crate::board::handle(sh, conn, r).await,
        r @ (Req::Whoami
        | Req::Register { .. }
        | Req::BootstrapOwner
        | Req::Shutdown
        | Req::ChannelCreate { .. }
        | Req::ChannelAdd { .. }
        | Req::ChannelRemove { .. }
        | Req::ChannelRole { .. }
        | Req::ChannelRotate { .. }
        | Req::ChannelList
        | Req::ChannelInfo { .. }
        | Req::ChannelKey { .. }
        | Req::RoleGrant { .. }
        | Req::Revoke { .. }) => crate::org::handle(sh, conn, r).await,
    }
}

/// The answer for a request routed to a module that does not handle it.
pub fn unrouted() -> ErrorBody {
    ErrorBody::new(ErrorCode::Internal, "unrouted request")
}

/// The fixed answer for any failure whose detail must not reach the client.
fn internal() -> ErrorBody {
    ErrorBody::new(ErrorCode::Internal, "internal error")
}

/// Log the store fault with its detail; the client gets only the fixed message.
fn internal_logged(e: &StoreError) -> ErrorBody {
    tracing::error!(error = %e, "store error");
    internal()
}

/// StoreError -> ErrorBody (see SPEC "Error mapping"). Internal errors are logged
/// with tracing::error! and answered with the fixed message "internal error".
pub fn store_error(e: &StoreError) -> ErrorBody {
    match e {
        StoreError::Sql(_) | StoreError::Io(_) | StoreError::BadState(_) => internal_logged(e),
        StoreError::Transition(TransitionError::Forbidden(_)) => {
            ErrorBody::new(ErrorCode::Forbidden, e.to_string())
        }
        StoreError::Transition(_) => ErrorBody::new(ErrorCode::Invalid, e.to_string()),
        StoreError::NotFound(_) => ErrorBody::new(ErrorCode::NotFound, e.to_string()),
        StoreError::NotMember(_) => ErrorBody::new(ErrorCode::NotMember, e.to_string()),
        StoreError::Crypto(_) => ErrorBody::new(ErrorCode::Crypto, e.to_string()),
        StoreError::Invalid(m) => match *m {
            "blob quota exceeded" => ErrorBody::new(ErrorCode::Quota, e.to_string()),
            "blob too large" => ErrorBody::new(ErrorCode::TooLarge, e.to_string()),
            "unknown identity" | "identity revoked" => {
                ErrorBody::new(ErrorCode::Forbidden, e.to_string())
            }
            // Server-side faults the store reports as Invalid: never echo them.
            "internal panic"
            | "corrupt role in database"
            | "blob file corrupt"
            | "blob file missing"
            | "blob hash malformed" => internal_logged(e),
            _ => ErrorBody::new(ErrorCode::Invalid, e.to_string()),
        },
    }
}

/// DaemonError -> ErrorBody: Store(e) -> store_error; ActorGone -> ShuttingDown;
/// Proto(e) -> proto_error; anything else -> Internal "internal error" (logged).
pub fn daemon_error(e: DaemonError) -> ErrorBody {
    match &e {
        DaemonError::Store(s) => store_error(s),
        DaemonError::ActorGone => ErrorBody::new(ErrorCode::ShuttingDown, e.to_string()),
        DaemonError::Proto(p) => proto_error(p),
        other => {
            tracing::error!(error = %other, "daemon error");
            internal()
        }
    }
}

/// ProtoError -> ErrorBody: Encoding(f) -> BadRequest "invalid <f>"; Json -> BadRequest;
/// Crypto -> BadRequest; FrameTooLarge -> TooLarge; Io -> Internal.
pub fn proto_error(e: &ProtoError) -> ErrorBody {
    match e {
        ProtoError::Encoding(_) | ProtoError::Json(_) | ProtoError::Crypto(_) => {
            ErrorBody::new(ErrorCode::BadRequest, e.to_string())
        }
        ProtoError::FrameTooLarge(_) => ErrorBody::new(ErrorCode::TooLarge, e.to_string()),
        ProtoError::Io(_) => {
            tracing::error!(error = %e, "protocol io error");
            internal()
        }
    }
}

/// Forbidden "signed by a different identity than the connection" unless `actor` == conn.pubkey.
pub fn signed_by_conn(conn: &Conn, actor: &Pub) -> Result<(), ErrorBody> {
    if conn.pubkey == *actor {
        Ok(())
    } else {
        Err(ErrorBody::new(
            ErrorCode::Forbidden,
            "signed by a different identity than the connection",
        ))
    }
}

/// Decode an OpWire (proto_error on failure) and require it is signed by the connection.
pub fn op_from_wire(conn: &Conn, op: &OpWire) -> Result<SignedOp, ErrorBody> {
    let signed = op.to_op().map_err(|e| proto_error(&e))?;
    signed_by_conn(conn, &signed.actor)?;
    Ok(signed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agora_core::{CardState, Role};
    use agora_crypto::CryptoError;

    fn conn(pubkey: Pub) -> Conn {
        Conn {
            pubkey,
            uid: 501,
            label: "test".into(),
        }
    }

    fn assert_body(body: &ErrorBody, code: ErrorCode, message: &str) {
        assert_eq!(body.code, code);
        assert_eq!(body.message, message);
    }

    #[test]
    fn store_sql_error_is_internal_with_fixed_message() {
        let dir = tempfile::tempdir().expect("tempdir");
        // The parent directory does not exist, so SQLite cannot open the file.
        // Store is not Debug, so drop the Ok value before expect_err.
        let err = agora_store::Store::open(&dir.path().join("missing/agora.db"))
            .map(|_| ())
            .expect_err("open must fail");
        assert!(matches!(err, StoreError::Sql(_)));
        assert_body(&store_error(&err), ErrorCode::Internal, "internal error");
    }

    #[test]
    fn store_io_error_is_internal_with_fixed_message() {
        let err = StoreError::Io(std::io::Error::other("disk gone"));
        assert_body(&store_error(&err), ErrorCode::Internal, "internal error");
    }

    #[test]
    fn store_bad_state_is_internal_without_leaking_the_row_value() {
        let err = StoreError::BadState("secret-state".into());
        assert_body(&store_error(&err), ErrorCode::Internal, "internal error");
    }

    #[test]
    fn transition_forbidden_maps_to_forbidden() {
        let err = StoreError::from(TransitionError::Forbidden(Role::Observer));
        let body = store_error(&err);
        assert_body(&body, ErrorCode::Forbidden, &err.to_string());
    }

    #[test]
    fn other_transition_errors_map_to_invalid_with_display_message() {
        let illegal = StoreError::from(TransitionError::Illegal {
            from: CardState::Ready,
            to: CardState::Done,
        });
        let evidence = StoreError::from(TransitionError::EvidenceRequired);
        let review = StoreError::from(TransitionError::ReviewRequired);
        assert_body(
            &store_error(&illegal),
            ErrorCode::Invalid,
            &illegal.to_string(),
        );
        assert_body(
            &store_error(&evidence),
            ErrorCode::Invalid,
            &evidence.to_string(),
        );
        assert_body(
            &store_error(&review),
            ErrorCode::Invalid,
            &review.to_string(),
        );
    }

    #[test]
    fn not_found_maps_to_not_found() {
        let err = StoreError::NotFound("card c1".into());
        assert_body(&store_error(&err), ErrorCode::NotFound, "card c1 not found");
    }

    #[test]
    fn not_member_maps_to_not_member() {
        let err = StoreError::NotMember("general".into());
        assert_body(
            &store_error(&err),
            ErrorCode::NotMember,
            "not a member of channel general",
        );
    }

    #[test]
    fn crypto_maps_to_crypto() {
        let err = StoreError::from(CryptoError::Decrypt);
        assert_body(&store_error(&err), ErrorCode::Crypto, "decryption failed");
    }

    #[test]
    fn blob_quota_message_maps_to_quota() {
        let err = StoreError::Invalid("blob quota exceeded");
        assert_body(&store_error(&err), ErrorCode::Quota, "blob quota exceeded");
    }

    #[test]
    fn blob_too_large_message_maps_to_too_large() {
        let err = StoreError::Invalid("blob too large");
        assert_body(&store_error(&err), ErrorCode::TooLarge, "blob too large");
    }

    #[test]
    fn unknown_or_revoked_identity_maps_to_forbidden() {
        let unknown = StoreError::Invalid("unknown identity");
        let revoked = StoreError::Invalid("identity revoked");
        assert_body(
            &store_error(&unknown),
            ErrorCode::Forbidden,
            "unknown identity",
        );
        assert_body(
            &store_error(&revoked),
            ErrorCode::Forbidden,
            "identity revoked",
        );
    }

    #[test]
    fn other_invalid_messages_map_to_invalid_with_the_same_text() {
        let err = StoreError::Invalid("channel key rotation pending");
        assert_body(
            &store_error(&err),
            ErrorCode::Invalid,
            "channel key rotation pending",
        );
    }

    #[test]
    fn store_server_side_invalid_messages_are_internal_without_detail() {
        for m in [
            "internal panic",
            "corrupt role in database",
            "blob file corrupt",
            "blob file missing",
            "blob hash malformed",
        ] {
            let body = store_error(&StoreError::Invalid(m));
            assert_eq!(body.code, ErrorCode::Internal, "{m}");
            assert_eq!(body.message, "internal error", "{m}");
        }
    }

    #[test]
    fn daemon_store_error_uses_store_mapping() {
        let body = daemon_error(DaemonError::Store(StoreError::NotMember("general".into())));
        assert_body(
            &body,
            ErrorCode::NotMember,
            "not a member of channel general",
        );
    }

    #[test]
    fn daemon_actor_gone_maps_to_shutting_down() {
        let body = daemon_error(DaemonError::ActorGone);
        assert_body(&body, ErrorCode::ShuttingDown, "store actor stopped");
    }

    #[test]
    fn daemon_proto_error_uses_proto_mapping() {
        let body = daemon_error(DaemonError::Proto(ProtoError::Encoding("pubkey")));
        assert_body(&body, ErrorCode::BadRequest, "invalid pubkey");
    }

    #[test]
    fn daemon_other_errors_map_to_internal() {
        let io = DaemonError::Io(std::io::Error::other("socket"));
        let config = DaemonError::Config("bad".into());
        assert_body(&daemon_error(io), ErrorCode::Internal, "internal error");
        assert_body(&daemon_error(config), ErrorCode::Internal, "internal error");
    }

    #[test]
    fn proto_encoding_names_the_field() {
        let body = proto_error(&ProtoError::Encoding("sig"));
        assert_body(&body, ErrorCode::BadRequest, "invalid sig");
    }

    #[test]
    fn proto_json_and_crypto_map_to_bad_request() {
        let json = serde_json::from_str::<u32>("x").expect_err("not a number");
        assert_eq!(
            proto_error(&ProtoError::Json(json)).code,
            ErrorCode::BadRequest
        );
        let crypto = ProtoError::Crypto(CryptoError::BadSignature);
        assert_body(
            &proto_error(&crypto),
            ErrorCode::BadRequest,
            "invalid signature",
        );
    }

    #[test]
    fn proto_frame_too_large_maps_to_too_large() {
        let body = proto_error(&ProtoError::FrameTooLarge(9_000_000));
        assert_body(
            &body,
            ErrorCode::TooLarge,
            "frame of 9000000 bytes exceeds the limit",
        );
    }

    #[test]
    fn proto_io_maps_to_internal() {
        let body = proto_error(&ProtoError::Io(std::io::Error::other("reset")));
        assert_body(&body, ErrorCode::Internal, "internal error");
    }

    #[test]
    fn signed_by_conn_accepts_the_connection_key() {
        let pk = [7u8; 32];
        assert!(signed_by_conn(&conn(pk), &pk).is_ok());
    }

    #[test]
    fn signed_by_conn_rejects_another_identity() {
        let err = signed_by_conn(&conn([7u8; 32]), &[8u8; 32])
            .expect_err("different key must be rejected");
        assert_eq!(err.code, ErrorCode::Forbidden);
    }
}
