//! chat, read, inbox, ack, react, fetch.

use crate::client::Client;
use crate::ops::{
    channel_key, now_ms, open_body, plain_message, sanitize, sealed_message, verify_view,
};
use crate::{ClientError, Ctx, Out};
use agora_crypto::{ChannelKey, attachment_aad, content_hash, pseudonym};
use agora_proto::{
    AttachmentWire, ChannelView, MAX_WAIT_MS, MessageView, ReactionWire, Reply, Req, b64, unb64,
    unhex_pk,
};
use agora_store::{Attachment, SignedReaction};
use serde_json::{Value, json};
use std::collections::HashMap;

/// Largest stored attachment `chat` accepts: the blob cap the daemon enforces.
pub const MAX_ATTACHMENT_BYTES: u64 = 5 * 1024 * 1024;
/// Bytes sealing adds to a plaintext: the XChaCha20-Poly1305 nonce (24) and tag (16).
const SEAL_OVERHEAD_BYTES: u64 = 24 + 16;
const ATTACHMENT_MIME: &str = "application/octet-stream";

/// One file to attach: its display name and content (the caller reads the file).
#[derive(Debug, Clone)]
pub struct AttachmentInput {
    pub name: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct ChatParams {
    pub channel: String,
    pub text: String,
    /// Reply in the thread of this message id.
    pub thread_of: Option<String>,
    /// Encrypted channels get each attachment sealed first.
    pub attachments: Vec<AttachmentInput>,
}

#[derive(Debug, Clone)]
pub struct ReadParams {
    pub channel: String,
    /// Only messages with seq greater than this.
    pub after: i64,
    pub limit: i64,
}

#[derive(Debug, Clone)]
pub struct InboxParams {
    /// Wait up to SECS (capped at 55) for a message when the inbox is empty.
    pub wait: Option<u64>,
    /// Only messages with seq greater than this (cursor from a previous call).
    pub after: i64,
    pub limit: i64,
}

#[derive(Debug, Clone)]
pub struct AckParams {
    pub channel: String,
    pub seq: i64,
}

#[derive(Debug, Clone)]
pub struct ReactParams {
    /// Message id (or blob hash / card id for kind file / card).
    pub target: String,
    pub emoji: String,
    /// message | file | card
    pub kind: String,
    pub remove: bool,
}

#[derive(Debug, Clone)]
pub struct FetchParams {
    pub hash: String,
    /// Decrypt with this channel's key (encrypted channels).
    pub channel: Option<String>,
    /// Key epoch of the message that carried the attachment (default: current).
    pub epoch: Option<i64>,
}

/// A fetched blob: the (decrypted) bytes plus the base JSON `{hash, bytes}` the caller
/// extends (the CLI adds `out`, the MCP tool adds `saved_to` / `base64`).
#[derive(Debug, Clone)]
pub struct Fetched {
    pub hash: String,
    pub bytes: Vec<u8>,
    pub json: Value,
}

pub async fn chat(ctx: &Ctx, p: ChatParams) -> Result<Out, ClientError> {
    let ChatParams {
        channel,
        text,
        thread_of,
        attachments: inputs,
    } = p;
    let mut client = Client::connect(ctx).await?;
    let info = channel_info(&mut client, &channel).await?;
    let key = if info.encrypted {
        Some(channel_key(&mut client, &ctx.identity, &channel, None).await?)
    } else {
        None
    };

    let limit = attachment_limit(key.is_some());
    check_attachments(&inputs, limit)?;
    let mut attachments = Vec::with_capacity(inputs.len());
    for AttachmentInput { name, bytes: data } in inputs {
        let bytes = match &key {
            Some((epoch, k)) => k.seal(&attachment_aad(&channel, *epoch), &data)?,
            None => data,
        };
        let Reply::BlobHash { hash } = client.call(Req::PutBlob { bytes: b64(&bytes) }).await?
        else {
            return Err(ClientError::UnexpectedReply);
        };
        attachments.push(Attachment {
            hash,
            name,
            mime: ATTACHMENT_MIME.into(),
        });
    }

    let msg = match &key {
        Some((epoch, k)) => sealed_message(
            &ctx.identity,
            &channel,
            thread_of,
            *epoch,
            k,
            text.as_bytes(),
            attachments,
        )?,
        None => plain_message(
            &ctx.identity,
            &channel,
            thread_of,
            text.as_bytes(),
            attachments,
        ),
    };
    let id = msg.id.clone();
    let Reply::Posted { seq } = client.call(Req::Post { msg }).await? else {
        return Err(ClientError::UnexpectedReply);
    };
    Ok(Out {
        json: json!({"seq": seq, "id": id, "channel": channel}),
        text: format!("posted {id} (seq {seq})"),
    })
}

pub async fn read(ctx: &Ctx, a: ReadParams) -> Result<Out, ClientError> {
    let mut client = Client::connect(ctx).await?;
    let Reply::Messages { messages } = client
        .call(Req::Read {
            channel: a.channel,
            after: a.after,
            limit: a.limit,
        })
        .await?
    else {
        return Err(ClientError::UnexpectedReply);
    };
    let (rendered, text) = render(&mut client, ctx, messages).await?;
    Ok(Out {
        json: json!({"messages": rendered}),
        text,
    })
}

/// `inbox`. Without `--wait`, `--after` still selects messages past the cursor: `Inbox`
/// has no cursor field, so that case uses `InboxWait` with `timeout_ms: 0`, which answers
/// at once instead of waiting.
pub async fn inbox(ctx: &Ctx, a: InboxParams) -> Result<Out, ClientError> {
    let mut client = Client::connect(ctx).await?;
    inbox_with(&mut client, ctx, a).await
}

/// `inbox` over an already connected `client` (the Claude Code hooks connect without spawning
/// the daemon). Same request selection and output as `inbox`; the final `inbox` is
/// `Client::connect` + this function.
pub async fn inbox_with(
    client: &mut Client,
    ctx: &Ctx,
    a: InboxParams,
) -> Result<Out, ClientError> {
    let req = match a.wait {
        Some(secs) => Req::InboxWait {
            after: a.after,
            timeout_ms: wait_timeout_ms(secs),
            limit: a.limit,
        },
        None if a.after > 0 => Req::InboxWait {
            after: a.after,
            timeout_ms: 0,
            limit: a.limit,
        },
        None => Req::Inbox { limit: a.limit },
    };
    let Reply::Inbox {
        messages,
        cursor,
        timed_out,
    } = client.call(req).await?
    else {
        return Err(ClientError::UnexpectedReply);
    };
    let (rendered, text) = render(client, ctx, messages).await?;
    Ok(Out {
        json: json!({"messages": rendered, "cursor": cursor, "timed_out": timed_out}),
        text,
    })
}

pub async fn ack(ctx: &Ctx, a: AckParams) -> Result<Out, ClientError> {
    let AckParams { channel, seq } = a;
    let mut client = Client::connect(ctx).await?;
    let Reply::Unit = client
        .call(Req::Ack {
            channel: channel.clone(),
            seq,
        })
        .await?
    else {
        return Err(ClientError::UnexpectedReply);
    };
    Ok(Out {
        json: json!({"channel": channel, "seq": seq}),
        text: format!("acked {channel} through seq {seq}"),
    })
}

pub async fn react(ctx: &Ctx, a: ReactParams) -> Result<Out, ClientError> {
    let ReactParams {
        target,
        emoji,
        kind,
        remove,
    } = a;
    let mut client = Client::connect(ctx).await?;
    let reaction = SignedReaction {
        kind: kind.clone(),
        target: target.clone(),
        by: [0; 32],
        emoji: emoji.clone(),
        remove,
        ts: now_ms(),
        sig: vec![],
    }
    .sign(&ctx.identity);
    let Reply::Unit = client
        .call(Req::React {
            reaction: ReactionWire::from_reaction(&reaction),
        })
        .await?
    else {
        return Err(ClientError::UnexpectedReply);
    };
    let verb = if remove { "removed" } else { "added" };
    Ok(Out {
        json: json!({"kind": kind, "target": target, "emoji": emoji, "remove": remove}),
        text: format!("{verb} {emoji} on {kind} {target}"),
    })
}

pub async fn fetch(ctx: &Ctx, p: FetchParams) -> Result<Fetched, ClientError> {
    let FetchParams {
        hash,
        channel,
        epoch,
    } = p;
    if !is_hash(&hash) {
        return Err(ClientError::Usage(
            "hash must be 64 lowercase hex characters".into(),
        ));
    }
    let mut client = Client::connect(ctx).await?;
    let Reply::Blob { bytes } = client.call(Req::GetBlob { hash: hash.clone() }).await? else {
        return Err(ClientError::UnexpectedReply);
    };
    let stored = unb64(&bytes, "bytes")?;
    check_blob_hash(&stored, &hash)?;
    let data = match channel {
        Some(name) => {
            let info = channel_info(&mut client, &name).await?;
            if info.encrypted {
                let (epoch, key) = channel_key(&mut client, &ctx.identity, &name, epoch).await?;
                key.open(&attachment_aad(&name, epoch), &stored)?
            } else {
                stored
            }
        }
        None => stored,
    };
    let json = json!({"hash": hash, "bytes": data.len()});
    Ok(Fetched {
        hash,
        bytes: data,
        json,
    })
}

/// The daemon's view of a channel (ChannelInfo); any other reply is unexpected.
async fn channel_info(client: &mut Client, channel: &str) -> Result<ChannelView, ClientError> {
    let Reply::Channel { channel: info, .. } = client
        .call(Req::ChannelInfo {
            channel: channel.to_string(),
        })
        .await?
    else {
        return Err(ClientError::UnexpectedReply);
    };
    Ok(info)
}

/// A content hash as the daemon and the store print it: 64 lowercase hex characters.
fn is_hash(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The hash names the stored (possibly sealed) bytes. Checked before anything is decrypted
/// or written.
fn check_blob_hash(stored: &[u8], hash: &str) -> Result<(), ClientError> {
    if content_hash(stored) == hash {
        Ok(())
    } else {
        Err(ClientError::Usage("blob does not match its hash".into()))
    }
}

/// `inbox --wait` seconds as the daemon's timeout: capped at MAX_WAIT_MS.
fn wait_timeout_ms(secs: u64) -> u64 {
    secs.min(MAX_WAIT_MS / 1000) * 1000
}

/// Largest attachment plaintext `chat` accepts. An encrypted channel stores the sealed
/// bytes, which are SEAL_OVERHEAD_BYTES longer, and the stored blob must still fit the cap.
fn attachment_limit(encrypted: bool) -> u64 {
    if encrypted {
        MAX_ATTACHMENT_BYTES - SEAL_OVERHEAD_BYTES
    } else {
        MAX_ATTACHMENT_BYTES
    }
}

/// A plaintext of `len` bytes must fit `limit`; the error names the attachment.
fn check_attachment_size(name: &str, len: usize, limit: u64) -> Result<(), ClientError> {
    if u64::try_from(len).is_ok_and(|n| n <= limit) {
        Ok(())
    } else {
        Err(ClientError::Usage(format!(
            "attachment {name} is larger than {limit} bytes"
        )))
    }
}

/// Every attachment must fit `limit`, checked before any of them is uploaded, so a rejected
/// message leaves no orphaned blobs behind.
fn check_attachments(inputs: &[AttachmentInput], limit: u64) -> Result<(), ClientError> {
    inputs
        .iter()
        .try_for_each(|a| check_attachment_size(&a.name, a.bytes.len(), limit))
}

/// Verify, decrypt and format messages for display. Returns the JSON objects and the
/// text lines (joined by newlines). A message whose text cannot be recovered has
/// text null.
async fn render(
    client: &mut Client,
    ctx: &Ctx,
    views: Vec<MessageView>,
) -> Result<(Vec<Value>, String), ClientError> {
    let mut keys: HashMap<(String, i64), Option<ChannelKey>> = HashMap::new();
    let mut rendered = Vec::with_capacity(views.len());
    let mut lines = Vec::with_capacity(views.len());
    for view in &views {
        let verified = verify_view(view);
        let bytes = if view.msg.encrypted {
            let key = cached_key(client, ctx, &mut keys, &view.msg.channel, view.msg.epoch).await?;
            key.and_then(|k| open_body(k, view).ok())
        } else {
            unb64(&view.msg.body, "body").ok()
        };
        let text = bytes.map(|b| String::from_utf8_lossy(&b).into_owned());
        let pseudo = pseudonym_of(&view.msg.sender);
        lines.push(format_line(
            view.seq,
            &view.msg.channel,
            &pseudo,
            text.as_deref(),
            verified,
            &view.msg.attachments,
        ));
        rendered.push(message_json(view, &pseudo, text.as_deref(), verified));
    }
    Ok((rendered, lines.join("\n")))
}

/// The key of (channel, epoch), fetched at most once per command. A key the daemon
/// refuses (not a member, unknown epoch, malformed wrap) is cached as None. Transport
/// failures propagate instead, so a dead connection is not reported as undecryptable.
async fn cached_key<'a>(
    client: &mut Client,
    ctx: &Ctx,
    keys: &'a mut HashMap<(String, i64), Option<ChannelKey>>,
    channel: &str,
    epoch: i64,
) -> Result<Option<&'a ChannelKey>, ClientError> {
    let slot = (channel.to_string(), epoch);
    if !keys.contains_key(&slot) {
        let fetched = match channel_key(client, &ctx.identity, channel, Some(epoch)).await {
            Ok((_, key)) => Some(key),
            Err(
                e @ (ClientError::Daemon(_) | ClientError::Io(_) | ClientError::UnexpectedReply),
            ) => {
                return Err(e);
            }
            Err(_) => None,
        };
        keys.insert(slot.clone(), fetched);
    }
    Ok(keys.get(&slot).and_then(Option::as_ref))
}

/// Pseudonym of a hex sender key; the sender string itself when it does not decode.
fn pseudonym_of(sender: &str) -> String {
    match unhex_pk(sender, "sender") {
        Ok(pk) => pseudonym(&pk),
        Err(_) => sender.to_string(),
    }
}

/// `[<seq>] <channel> <pseudonym>: <text>`, then ` [att <name> <hash>]` per attachment,
/// then " (UNVERIFIED)" when the signature fails. Every untrusted string (channel,
/// pseudonym, text, attachment name and hash) is sanitized, so a message cannot forge a
/// line break or a terminal escape.
fn format_line(
    seq: i64,
    channel: &str,
    pseudonym: &str,
    text: Option<&str>,
    verified: bool,
    attachments: &[AttachmentWire],
) -> String {
    let body = match text {
        Some(t) => sanitize(t),
        None => "(cannot decrypt)".to_string(),
    };
    let atts: String = attachments
        .iter()
        .map(|a| format!(" [att {} {}]", sanitize(&a.name), sanitize(&a.hash)))
        .collect();
    let tag = if verified { "" } else { " (UNVERIFIED)" };
    format!(
        "[{seq}] {} {}: {body}{atts}{tag}",
        sanitize(channel),
        sanitize(pseudonym)
    )
}

fn message_json(view: &MessageView, pseudonym: &str, text: Option<&str>, verified: bool) -> Value {
    let m = &view.msg;
    let attachments: Vec<Value> = m
        .attachments
        .iter()
        .map(|a| json!({"hash": a.hash, "name": a.name, "mime": a.mime}))
        .collect();
    json!({
        "seq": view.seq,
        "id": m.id,
        "channel": m.channel,
        "thread_of": m.thread_of,
        "sender": m.sender,
        "pseudonym": pseudonym,
        "ts": m.ts,
        "encrypted": m.encrypted,
        "epoch": m.epoch,
        "verified": verified,
        "text": text,
        "attachments": attachments,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use agora_proto::{AttachmentWire, MessageWire};

    fn sample_view(sender: &str) -> MessageView {
        MessageView {
            seq: 4,
            msg: MessageWire {
                id: "m-1".into(),
                channel: "c1".into(),
                thread_of: None,
                sender: sender.into(),
                ts: 99,
                encrypted: true,
                epoch: 2,
                body: String::new(),
                attachments: vec![AttachmentWire {
                    hash: "ab".repeat(32),
                    name: "a.txt".into(),
                    mime: "text/plain".into(),
                }],
                sig: String::new(),
            },
        }
    }

    fn attachment(name: &str, hash: &str) -> AttachmentWire {
        AttachmentWire {
            hash: hash.into(),
            name: name.into(),
            mime: ATTACHMENT_MIME.into(),
        }
    }

    #[test]
    fn format_line_shows_text_and_marks_unverified() {
        assert_eq!(
            format_line(3, "c1", "alice", Some("hi"), true, &[]),
            "[3] c1 alice: hi"
        );
        assert_eq!(
            format_line(3, "c1", "alice", Some("hi"), false, &[]),
            "[3] c1 alice: hi (UNVERIFIED)"
        );
    }

    #[test]
    fn format_line_marks_undecryptable_text() {
        assert_eq!(
            format_line(9, "c2", "bob", None, true, &[]),
            "[9] c2 bob: (cannot decrypt)"
        );
    }

    #[test]
    fn format_line_escapes_control_characters_in_untrusted_strings() {
        assert_eq!(
            format_line(1, "c\n1", "al\x1b[31mice", Some("a\nb\x1b[31m"), true, &[]),
            "[1] c\\n1 al\\u{1b}[31mice: a\\nb\\u{1b}[31m"
        );
    }

    #[test]
    fn format_line_lists_attachments_before_the_unverified_tag() {
        let atts = [attachment("a.txt", &"ab".repeat(32))];
        assert_eq!(
            format_line(3, "c1", "alice", Some("hi"), false, &atts),
            format!(
                "[3] c1 alice: hi [att a.txt {}] (UNVERIFIED)",
                "ab".repeat(32)
            )
        );
    }

    #[test]
    fn format_line_sanitizes_attachment_names() {
        let atts = [attachment("evil\nname", &"cd".repeat(32))];
        assert_eq!(
            format_line(3, "c1", "alice", None, true, &atts),
            format!(
                "[3] c1 alice: (cannot decrypt) [att evil\\nname {}]",
                "cd".repeat(32)
            )
        );
    }

    #[test]
    fn is_hash_accepts_only_64_lowercase_hex() {
        assert!(is_hash(&"0f".repeat(32)));
        assert!(!is_hash(&"0F".repeat(32)), "uppercase is refused");
        assert!(!is_hash(&"0f".repeat(31)), "short is refused");
        assert!(
            !is_hash(&format!("{}g", "0f".repeat(31))),
            "non-hex is refused"
        );
        assert!(!is_hash(""), "empty is refused");
    }

    #[test]
    fn check_blob_hash_compares_against_the_stored_bytes() {
        let stored = b"sealed bytes";
        let hash = content_hash(stored);
        assert!(check_blob_hash(stored, &hash).is_ok());
        assert!(matches!(
            check_blob_hash(b"tampered", &hash),
            Err(ClientError::Usage(m)) if m == "blob does not match its hash"
        ));
    }

    #[test]
    fn wait_timeout_ms_caps_at_the_protocol_maximum() {
        assert_eq!(wait_timeout_ms(0), 0);
        assert_eq!(wait_timeout_ms(5), 5_000);
        assert_eq!(wait_timeout_ms(55), 55_000);
        assert_eq!(wait_timeout_ms(3_600), 55_000);
        assert_eq!(wait_timeout_ms(u64::MAX), 55_000);
    }

    #[test]
    fn attachment_limit_reserves_room_for_the_seal_when_encrypted() {
        assert_eq!(attachment_limit(false), 5 * 1024 * 1024);
        assert_eq!(attachment_limit(true), 5 * 1024 * 1024 - 40);
    }

    #[test]
    fn pseudonym_of_decodes_hex_and_passes_through_garbage() {
        let pk = [7u8; 32];
        assert_eq!(pseudonym_of(&agora_store::hex(&pk)), pseudonym(&pk));
        assert_eq!(pseudonym_of("not-a-key"), "not-a-key");
    }

    #[test]
    fn message_json_has_the_spec_fields_and_null_text() {
        let view = sample_view(&"cd".repeat(32));
        let v = message_json(&view, "p", None, false);
        assert_eq!(v["seq"], 4);
        assert_eq!(v["id"], "m-1");
        assert_eq!(v["thread_of"], Value::Null);
        assert_eq!(v["pseudonym"], "p");
        assert_eq!(v["ts"], 99);
        assert_eq!(v["encrypted"], true);
        assert_eq!(v["epoch"], 2);
        assert_eq!(v["verified"], false);
        assert_eq!(v["text"], Value::Null);
        assert_eq!(v["attachments"][0]["name"], "a.txt");
        assert_eq!(v["attachments"][0]["mime"], "text/plain");
    }

    #[test]
    fn attachment_size_check_accepts_the_limit_and_names_the_attachment_beyond_it() {
        assert!(check_attachment_size("exact.bin", 10, 10).is_ok());
        assert!(check_attachment_size("empty.bin", 0, 10).is_ok());
        let limit = attachment_limit(true);
        let over = usize::try_from(limit + 1).expect("fits usize");
        let expected = format!("attachment sealed.bin is larger than {limit} bytes");
        assert!(matches!(
            check_attachment_size("sealed.bin", over, limit),
            Err(ClientError::Usage(m)) if m == expected
        ));
    }

    #[test]
    fn attachment_batch_is_refused_when_any_member_is_over_the_limit() {
        let input = |name: &str, len: usize| AttachmentInput {
            name: name.to_string(),
            bytes: vec![0; len],
        };
        assert!(check_attachments(&[input("a.bin", 4), input("b.bin", 4)], 4).is_ok());
        let batch = [input("small.bin", 1), input("big.bin", 5)];
        assert!(matches!(
            check_attachments(&batch, 4),
            Err(ClientError::Usage(m)) if m == "attachment big.bin is larger than 4 bytes"
        ));
    }
}
