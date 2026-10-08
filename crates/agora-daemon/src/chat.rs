//! Messages, blobs, reads, inbox, acks, reactions (see SPEC "Handlers: chat").

use crate::dispatch::{daemon_error, proto_error, signed_by_conn, unrouted};
use crate::{Conn, Shared};
use agora_proto::{EmojiCount, ErrorBody, ErrorCode, Event, MessageView, Reply, Req, b64, unb64};
use agora_store::hex;

/// Handles PutBlob, GetBlob, Post, Read, Inbox, Ack, React, ReactionCounts;
/// anything else -> `Err(dispatch::unrouted())`.
pub async fn handle(sh: &Shared, conn: &Conn, req: Req) -> Result<Reply, ErrorBody> {
    let pk = conn.pubkey;
    match req {
        Req::PutBlob { bytes: encoded } => {
            let max = sh.config.max_blob_bytes;
            // Base64 turns 3 bytes into 4 chars and padding removes at most 2 decoded bytes,
            // so a longer string is certainly over the cap: refuse it before decoding.
            if encoded.len() / 4 * 3 > max.saturating_add(2) {
                return Err(ErrorBody::new(ErrorCode::TooLarge, "blob too large"));
            }
            let bytes = unb64(&encoded, "bytes").map_err(|e| proto_error(&e))?;
            drop(encoded);
            if bytes.len() > max {
                return Err(ErrorBody::new(ErrorCode::TooLarge, "blob too large"));
            }
            let size = bytes.len();
            let now = crate::now_ms();
            let quota = sh.config.blob_quota_bytes;
            let hash = sh
                .writer
                .exec(move |s| {
                    let hash = s.put_blob_for(now, &pk, &bytes, max, quota)?;
                    Ok((hash.clone(), vec![Event::BlobStored { hash }]))
                })
                .await
                .map_err(daemon_error)?;
            tracing::debug!(op = "put_blob", bytes = size, "blob stored");
            Ok(Reply::BlobHash { hash })
        }
        Req::GetBlob { hash } => {
            let found = sh
                .reader
                .exec(move |s| {
                    s.role_of(&pk)?;
                    s.get_blob(&hash)
                })
                .await
                .map_err(daemon_error)?;
            let bytes = found.ok_or_else(|| ErrorBody::new(ErrorCode::NotFound, "blob"))?;
            tracing::debug!(op = "get_blob", bytes = bytes.len(), "blob read");
            Ok(Reply::Blob { bytes: b64(&bytes) })
        }
        Req::Post { msg } => {
            let m = msg.to_message().map_err(|e| proto_error(&e))?;
            signed_by_conn(conn, &m.sender)?;
            let now = crate::now_ms();
            let seq = sh
                .writer
                .exec(move |s| {
                    let seq = s.post_message(now, &m)?;
                    let channel = m.channel.clone();
                    Ok((seq, vec![Event::MessageAppended { channel, seq }]))
                })
                .await
                .map_err(daemon_error)?;
            tracing::debug!(op = "post", seq, "message appended");
            Ok(Reply::Posted { seq })
        }
        Req::Read {
            channel,
            after,
            limit,
        } => {
            let rows = sh
                .reader
                .exec(move |s| s.read_channel(&pk, &channel, after, limit))
                .await
                .map_err(daemon_error)?;
            tracing::debug!(op = "read", count = rows.len(), "messages read");
            let messages = rows.iter().map(MessageView::from_row).collect();
            Ok(Reply::Messages { messages })
        }
        Req::Inbox { limit } => {
            let rows = sh
                .reader
                .exec(move |s| s.inbox(&pk, limit))
                .await
                .map_err(daemon_error)?;
            tracing::debug!(op = "inbox", count = rows.len(), "inbox read");
            let messages: Vec<MessageView> = rows
                .iter()
                .map(|(_, row)| MessageView::from_row(row))
                .collect();
            let cursor = messages.iter().map(|m| m.seq).max().unwrap_or(0);
            Ok(Reply::Inbox {
                messages,
                cursor,
                timed_out: false,
            })
        }
        Req::Ack { channel, seq } => {
            let cursor = sh
                .writer
                .exec(move |s| {
                    let cursor = s.ack(&pk, &channel, seq)?;
                    let reader = hex(&pk);
                    Ok((
                        cursor,
                        vec![Event::Acked {
                            reader,
                            channel,
                            seq: cursor,
                        }],
                    ))
                })
                .await
                .map_err(daemon_error)?;
            tracing::debug!(op = "ack", cursor, "ack recorded");
            Ok(Reply::Unit)
        }
        Req::React { reaction } => {
            let r = reaction.to_reaction().map_err(|e| proto_error(&e))?;
            signed_by_conn(conn, &r.by)?;
            let now = crate::now_ms();
            let event = Event::ReactionChanged {
                kind: r.kind.clone(),
                target: r.target.clone(),
            };
            sh.writer
                .exec(move |s| {
                    s.react(now, &r)?;
                    Ok(((), vec![event]))
                })
                .await
                .map_err(daemon_error)?;
            tracing::debug!(op = "react", "reaction recorded");
            Ok(Reply::Unit)
        }
        Req::ReactionCounts { kind, target } => {
            let rows = sh
                .reader
                .exec(move |s| s.reaction_counts_for(&pk, &kind, &target))
                .await
                .map_err(daemon_error)?;
            let counts = rows
                .into_iter()
                .map(|(emoji, count)| EmojiCount { emoji, count })
                .collect();
            Ok(Reply::Counts { counts })
        }
        _ => Err(unrouted()),
    }
}
