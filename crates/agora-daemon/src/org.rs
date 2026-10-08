//! Identity, registration policy, shutdown, channels, roles (see SPEC "Handlers: org").

use crate::dispatch::{daemon_error, op_from_wire, proto_error, unrouted};
use crate::{Conn, Shared, now_ms};
use agora_crypto::pseudonym;
use agora_proto::{
    ChannelView, ErrorBody, ErrorCode, Event, MemberView, OpWire, Reply, Req, b64, keys_from_wire,
    unb64,
};
use agora_store::{IdentityStatus, Pub, hex};
use std::time::Duration;

/// Longest label `Register` accepts, in bytes.
const MAX_LABEL_BYTES: usize = 64;

/// Handles Whoami, Register, BootstrapOwner, Shutdown, ChannelCreate, ChannelAdd,
/// ChannelRemove, ChannelRole, ChannelRotate, ChannelList, ChannelInfo, ChannelKey,
/// RoleGrant, Revoke; anything else -> `Err(dispatch::unrouted())`.
pub async fn handle(sh: &Shared, conn: &Conn, req: Req) -> Result<Reply, ErrorBody> {
    match req {
        Req::Whoami => whoami(sh, conn.pubkey).await,
        Req::Register { label } => register(sh, conn, label).await,
        Req::BootstrapOwner => bootstrap_owner(sh, conn).await,
        Req::Shutdown => shutdown(sh, conn).await,
        Req::ChannelCreate { op, keys } => {
            let op = op_from_wire(conn, &op)?;
            let keys = keys_from_wire(&keys).map_err(|e| proto_error(&e))?;
            let now = now_ms();
            let channel = op.subject.clone();
            sh.writer
                .exec(move |s| {
                    s.create_channel(now, &op, &keys)?;
                    Ok((Reply::Unit, vec![Event::ChannelChanged { channel }]))
                })
                .await
                .map_err(daemon_error)
        }
        Req::ChannelAdd { op, wrapped } => {
            let op = op_from_wire(conn, &op)?;
            let wrapped = wrapped
                .as_deref()
                .map(|w| unb64(w, "wrapped"))
                .transpose()
                .map_err(|e| proto_error(&e))?;
            let now = now_ms();
            let channel = op.subject.clone();
            sh.writer
                .exec(move |s| {
                    s.add_member(now, &op, wrapped)?;
                    Ok((Reply::Unit, vec![Event::ChannelChanged { channel }]))
                })
                .await
                .map_err(daemon_error)
        }
        Req::ChannelRemove { op, keys } => {
            let op = op_from_wire(conn, &op)?;
            let keys = keys_from_wire(&keys).map_err(|e| proto_error(&e))?;
            let now = now_ms();
            let channel = op.subject.clone();
            sh.writer
                .exec(move |s| {
                    s.remove_member(now, &op, &keys)?;
                    Ok((Reply::Unit, vec![Event::ChannelChanged { channel }]))
                })
                .await
                .map_err(daemon_error)
        }
        Req::ChannelRole { op } => {
            let op = op_from_wire(conn, &op)?;
            let now = now_ms();
            let channel = op.subject.clone();
            sh.writer
                .exec(move |s| {
                    s.set_channel_role(now, &op)?;
                    Ok((Reply::Unit, vec![Event::ChannelChanged { channel }]))
                })
                .await
                .map_err(daemon_error)
        }
        Req::ChannelRotate { op, keys } => {
            let op = op_from_wire(conn, &op)?;
            let keys = keys_from_wire(&keys).map_err(|e| proto_error(&e))?;
            let now = now_ms();
            let channel = op.subject.clone();
            sh.writer
                .exec(move |s| {
                    let epoch = s.rotate_channel_key(now, &op, &keys)?;
                    Ok((
                        Reply::Epoch { epoch },
                        vec![Event::ChannelChanged { channel }],
                    ))
                })
                .await
                .map_err(daemon_error)
        }
        Req::ChannelList => {
            let me = conn.pubkey;
            sh.reader
                .exec(move |s| {
                    let channels: Vec<ChannelView> = s
                        .list_channels(&me)?
                        .iter()
                        .map(ChannelView::from_info)
                        .collect();
                    Ok(Reply::Channels { channels })
                })
                .await
                .map_err(daemon_error)
        }
        Req::ChannelInfo { channel } => {
            let me = conn.pubkey;
            sh.reader
                .exec(move |s| {
                    let (info, members) = s.channel_info(&me, &channel)?;
                    Ok(Reply::Channel {
                        channel: ChannelView::from_info(&info),
                        members: members
                            .iter()
                            .map(|(pk, role)| MemberView::from_member(pk, role))
                            .collect(),
                    })
                })
                .await
                .map_err(daemon_error)
        }
        Req::ChannelKey { channel, epoch } => {
            let me = conn.pubkey;
            sh.reader
                .exec(move |s| {
                    let (found, wrapper, wrapped) = s.member_key(&me, &channel, epoch)?;
                    Ok(Reply::Key {
                        epoch: found,
                        wrapper: hex(&wrapper),
                        wrapped: b64(&wrapped),
                    })
                })
                .await
                .map_err(daemon_error)
        }
        Req::RoleGrant { op } => {
            let op = op_from_wire(conn, &op)?;
            let now = now_ms();
            let subject = op.subject.clone();
            sh.writer
                .exec(move |s| {
                    s.grant_role(now, &op)?;
                    Ok((
                        Reply::Unit,
                        vec![Event::IdentityChanged { pubkey: subject }],
                    ))
                })
                .await
                .map_err(daemon_error)
        }
        Req::Revoke { op } => revoke(sh, conn, &op).await,
        _ => Err(unrouted()),
    }
}

/// Registration state and owner existence of the connection's own key.
async fn whoami(sh: &Shared, pk: Pub) -> Result<Reply, ErrorBody> {
    sh.reader
        .exec(move |s| {
            let status = s.identity_status(&pk)?;
            let owner_exists = s.owner_exists()?;
            let (role, registered, revoked) = match status {
                IdentityStatus::Unknown => (None, false, false),
                IdentityStatus::Active(role) => (Some(role.as_str().to_string()), true, false),
                IdentityStatus::Revoked => (None, true, true),
            };
            Ok(Reply::Whoami {
                pubkey: hex(&pk),
                pseudonym: pseudonym(&pk),
                role,
                registered,
                revoked,
                owner_exists,
            })
        })
        .await
        .map_err(daemon_error)
}

/// Registers the connection key, subject to the local-mode `open_registration` policy.
/// A key that is already registered is a no-op: Unit, and no IdentityChanged is published.
/// Under closed registration only an already registered key gets Unit; an unknown one is
/// refused with RegistrationClosed.
async fn register(sh: &Shared, conn: &Conn, label: String) -> Result<Reply, ErrorBody> {
    let pk = conn.pubkey;
    if !sh.config.open_registration {
        let status = sh
            .reader
            .exec(move |s| s.identity_status(&pk))
            .await
            .map_err(daemon_error)?;
        if status == IdentityStatus::Unknown {
            return Err(ErrorBody::new(
                ErrorCode::RegistrationClosed,
                "registration is closed",
            ));
        }
        return Ok(Reply::Unit);
    }
    if label.len() > MAX_LABEL_BYTES {
        return Err(ErrorBody::new(ErrorCode::Invalid, "label exceeds 64 bytes"));
    }
    let now = now_ms();
    // The status is read on the writer, in the same serialized job as the insert, so the
    // IdentityChanged decision cannot race with a concurrent registration of the same key.
    let newly_registered = sh
        .writer
        .exec(move |s| {
            let fresh = s.identity_status(&pk)? == IdentityStatus::Unknown;
            s.register_identity(now, &pk, &label)?;
            let events = if fresh {
                vec![Event::IdentityChanged { pubkey: hex(&pk) }]
            } else {
                Vec::new()
            };
            Ok((fresh, events))
        })
        .await
        .map_err(daemon_error)?;
    if newly_registered {
        tracing::info!(pseudonym = %pseudonym(&pk), "identity registered");
    }
    Ok(Reply::Unit)
}

/// Makes the connection key the owner. Only a peer running as the daemon's own uid may
/// do this; the store refuses it once an owner exists. Works while registration is closed.
async fn bootstrap_owner(sh: &Shared, conn: &Conn) -> Result<Reply, ErrorBody> {
    if conn.uid != sh.daemon_uid {
        return Err(ErrorBody::new(
            ErrorCode::Forbidden,
            "owner bootstrap requires the daemon's uid",
        ));
    }
    let pk = conn.pubkey;
    let now = now_ms();
    let reply = sh
        .writer
        .exec(move |s| {
            s.bootstrap_owner(now, &pk)?;
            Ok((
                Reply::Unit,
                vec![Event::IdentityChanged { pubkey: hex(&pk) }],
            ))
        })
        .await
        .map_err(daemon_error)?;
    tracing::info!(pseudonym = %pseudonym(&pk), "owner bootstrapped");
    Ok(reply)
}

/// Answers Unit at once and cancels the daemon 50 ms later, so the reply is queued first.
/// A revoked key may not stop the daemon; unknown and active keys may (the peer uid is
/// already checked by the connection handshake).
async fn shutdown(sh: &Shared, conn: &Conn) -> Result<Reply, ErrorBody> {
    let pk = conn.pubkey;
    let status = sh
        .reader
        .exec(move |s| s.identity_status(&pk))
        .await
        .map_err(daemon_error)?;
    if status == IdentityStatus::Revoked {
        return Err(ErrorBody::new(
            ErrorCode::Forbidden,
            "a revoked identity may not shut down the daemon",
        ));
    }
    tracing::info!(pseudonym = %pseudonym(&pk), "shutdown requested");
    let token = sh.shutdown.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        token.cancel();
    });
    Ok(Reply::Unit)
}

/// Revokes the identity named by the signed op's subject.
async fn revoke(sh: &Shared, conn: &Conn, op: &OpWire) -> Result<Reply, ErrorBody> {
    let op = op_from_wire(conn, op)?;
    let now = now_ms();
    let subject = op.subject.clone();
    let event_subject = subject.clone();
    let reply = sh
        .writer
        .exec(move |s| {
            s.revoke_identity(now, &op)?;
            Ok((
                Reply::Unit,
                vec![Event::IdentityChanged {
                    pubkey: event_subject,
                }],
            ))
        })
        .await
        .map_err(daemon_error)?;
    tracing::info!(
        pseudonym = %pseudonym(&conn.pubkey),
        subject = %subject,
        "identity revoked"
    );
    Ok(reply)
}
