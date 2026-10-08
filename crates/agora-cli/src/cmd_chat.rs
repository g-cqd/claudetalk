//! chat, read, inbox, ack, react, fetch (see SPEC "CLI").

use crate::client::Client;
use crate::ops::{
    channel_key, now_ms, open_body, plain_message, sanitize, sealed_message, verify_view,
};
use crate::{AckArgs, ChatArgs, CliError, Ctx, FetchArgs, InboxArgs, Out, ReactArgs, ReadArgs};
use agora_crypto::{ChannelKey, attachment_aad, content_hash, pseudonym};
use agora_proto::{
    AttachmentWire, ChannelView, MAX_WAIT_MS, MessageView, ReactionWire, Reply, Req, b64, unb64,
    unhex_pk,
};
use agora_store::{Attachment, SignedReaction};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// Largest stored attachment `chat` accepts: the blob cap the daemon enforces.
const MAX_ATTACHMENT_BYTES: u64 = 5 * 1024 * 1024;
/// Bytes sealing adds to a plaintext: the XChaCha20-Poly1305 nonce (24) and tag (16).
const SEAL_OVERHEAD_BYTES: u64 = 24 + 16;
const ATTACHMENT_MIME: &str = "application/octet-stream";

pub async fn chat(ctx: &Ctx, a: ChatArgs) -> Result<Out, CliError> {
    let ChatArgs {
        channel,
        text,
        thread_of,
        attach,
    } = a;
    let mut client = Client::connect(ctx).await?;
    let info = channel_info(&mut client, &channel).await?;
    let key = if info.encrypted {
        Some(channel_key(&mut client, &ctx.identity, &channel, None).await?)
    } else {
        None
    };

    let limit = attachment_limit(key.is_some());
    let mut attachments = Vec::with_capacity(attach.len());
    for path in &attach {
        let (name, data) = read_attachment(path, limit)?;
        let bytes = match &key {
            Some((epoch, k)) => k.seal(&attachment_aad(&channel, *epoch), &data)?,
            None => data,
        };
        let Reply::BlobHash { hash } = client.call(Req::PutBlob { bytes: b64(&bytes) }).await?
        else {
            return Err(CliError::UnexpectedReply);
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
        return Err(CliError::UnexpectedReply);
    };
    Ok(Out {
        json: json!({"seq": seq, "id": id, "channel": channel}),
        text: format!("posted {id} (seq {seq})"),
    })
}

pub async fn read(ctx: &Ctx, a: ReadArgs) -> Result<Out, CliError> {
    let mut client = Client::connect(ctx).await?;
    let Reply::Messages { messages } = client
        .call(Req::Read {
            channel: a.channel,
            after: a.after,
            limit: a.limit,
        })
        .await?
    else {
        return Err(CliError::UnexpectedReply);
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
pub async fn inbox(ctx: &Ctx, a: InboxArgs) -> Result<Out, CliError> {
    let mut client = Client::connect(ctx).await?;
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
        return Err(CliError::UnexpectedReply);
    };
    let (rendered, text) = render(&mut client, ctx, messages).await?;
    Ok(Out {
        json: json!({"messages": rendered, "cursor": cursor, "timed_out": timed_out}),
        text,
    })
}

pub async fn ack(ctx: &Ctx, a: AckArgs) -> Result<Out, CliError> {
    let AckArgs { channel, seq } = a;
    let mut client = Client::connect(ctx).await?;
    let Reply::Unit = client
        .call(Req::Ack {
            channel: channel.clone(),
            seq,
        })
        .await?
    else {
        return Err(CliError::UnexpectedReply);
    };
    Ok(Out {
        json: json!({"channel": channel, "seq": seq}),
        text: format!("acked {channel} through seq {seq}"),
    })
}

pub async fn react(ctx: &Ctx, a: ReactArgs) -> Result<Out, CliError> {
    let ReactArgs {
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
        return Err(CliError::UnexpectedReply);
    };
    let verb = if remove { "removed" } else { "added" };
    Ok(Out {
        json: json!({"kind": kind, "target": target, "emoji": emoji, "remove": remove}),
        text: format!("{verb} {emoji} on {kind} {target}"),
    })
}

pub async fn fetch(ctx: &Ctx, a: FetchArgs) -> Result<Out, CliError> {
    let FetchArgs {
        hash,
        out,
        channel,
        epoch,
    } = a;
    if !is_hash(&hash) {
        return Err(CliError::Usage(
            "hash must be 64 lowercase hex characters".into(),
        ));
    }
    let mut client = Client::connect(ctx).await?;
    let Reply::Blob { bytes } = client.call(Req::GetBlob { hash: hash.clone() }).await? else {
        return Err(CliError::UnexpectedReply);
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
    write_output(&out, &data)?;
    Ok(Out {
        json: json!({"hash": hash, "bytes": data.len(), "out": out.display().to_string()}),
        text: format!("wrote {} bytes to {}", data.len(), out.display()),
    })
}

/// The daemon's view of a channel (ChannelInfo); any other reply is unexpected.
async fn channel_info(client: &mut Client, channel: &str) -> Result<ChannelView, CliError> {
    let Reply::Channel { channel: info, .. } = client
        .call(Req::ChannelInfo {
            channel: channel.to_string(),
        })
        .await?
    else {
        return Err(CliError::UnexpectedReply);
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
fn check_blob_hash(stored: &[u8], hash: &str) -> Result<(), CliError> {
    if content_hash(stored) == hash {
        Ok(())
    } else {
        Err(CliError::Usage("blob does not match its hash".into()))
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

/// Read an attachment of at most `limit` bytes. Returns (file name, bytes).
/// Reads one byte past the limit so a file that grows while read is still refused.
/// I/O failures name the path.
fn read_attachment(path: &Path, limit: u64) -> Result<(String, Vec<u8>), CliError> {
    let at = |e: std::io::Error| CliError::Usage(format!("{}: {e}", path.display()));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| {
            CliError::Usage(format!("attachment has no file name: {}", path.display()))
        })?;
    let mut data = Vec::new();
    std::fs::File::open(path)
        .map_err(at)?
        .take(limit + 1)
        .read_to_end(&mut data)
        .map_err(at)?;
    if u64::try_from(data.len()).is_ok_and(|n| n > limit) {
        return Err(CliError::Usage(format!(
            "attachment {name} is larger than {limit} bytes"
        )));
    }
    Ok((name, data))
}

/// Write fetched bytes to `path` as a private file: created 0600 or truncated, and never
/// through a symlink (O_NOFOLLOW). I/O failures name the path.
fn write_output(path: &Path, data: &[u8]) -> Result<(), CliError> {
    let at = |e: std::io::Error| CliError::Usage(format!("{}: {e}", path.display()));
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(at)?;
    let meta = file.metadata().map_err(at)?;
    if !meta.is_file() {
        return Err(CliError::Usage(format!(
            "{}: not a regular file",
            path.display()
        )));
    }
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(at)?;
    file.write_all(data).map_err(at)
}

/// Verify, decrypt and format messages for display. Returns the JSON objects and the
/// text lines (joined by newlines). A message whose text cannot be recovered has
/// text null.
async fn render(
    client: &mut Client,
    ctx: &Ctx,
    views: Vec<MessageView>,
) -> Result<(Vec<Value>, String), CliError> {
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
) -> Result<Option<&'a ChannelKey>, CliError> {
    let slot = (channel.to_string(), epoch);
    if !keys.contains_key(&slot) {
        let fetched = match channel_key(client, &ctx.identity, channel, Some(epoch)).await {
            Ok((_, key)) => Some(key),
            Err(e @ (CliError::Daemon(_) | CliError::Io(_) | CliError::UnexpectedReply)) => {
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
            Err(CliError::Usage(m)) if m == "blob does not match its hash"
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
    fn write_output_creates_a_private_file_and_truncates_an_existing_one() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("out.bin");
        write_output(&path, b"a much longer first write").expect("create");
        write_output(&path, b"short").expect("truncate");
        assert_eq!(std::fs::read(&path).expect("read"), b"short");
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn write_output_tightens_an_existing_world_readable_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("loose.bin");
        std::fs::write(&path, b"old").expect("create");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        write_output(&path, b"secret").expect("write");
        assert_eq!(std::fs::read(&path).expect("read"), b"secret");
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn write_output_refuses_a_fifo_without_blocking() {
        let dir = tempfile::tempdir().expect("temp dir");
        let fifo = dir.path().join("pipe.bin");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo");
        assert!(status.success());
        let err = write_output(&fifo, b"x").expect_err("fifo refused");
        assert!(matches!(err, CliError::Usage(m) if m.contains("pipe.bin")));
    }

    #[test]
    fn write_output_refuses_to_follow_a_symlink() {
        let dir = tempfile::tempdir().expect("temp dir");
        let victim = dir.path().join("victim.txt");
        let link = dir.path().join("link.bin");
        std::fs::write(&victim, b"keep").expect("write victim");
        std::os::unix::fs::symlink(&victim, &link).expect("symlink");
        let err = write_output(&link, b"overwrite").expect_err("symlink refused");
        assert!(matches!(err, CliError::Usage(m) if m.contains("link.bin")));
        assert_eq!(std::fs::read(&victim).expect("read victim"), b"keep");
    }

    #[test]
    fn io_failures_on_attach_and_output_name_the_path() {
        let missing = Path::new("/nonexistent-agora-dir/file.bin");
        let err = read_attachment(missing, 10).expect_err("missing file");
        assert!(matches!(err, CliError::Usage(m) if m.contains("file.bin:")));
        let err = write_output(missing, b"x").expect_err("missing dir");
        assert!(matches!(err, CliError::Usage(m) if m.contains("file.bin:")));
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
    fn read_attachment_accepts_the_limit_and_refuses_one_byte_more() {
        let dir = tempfile::tempdir().expect("temp dir");
        let at_limit = dir.path().join("exact.bin");
        let over = dir.path().join("over.bin");
        let limit = usize::try_from(MAX_ATTACHMENT_BYTES).expect("fits usize");
        std::fs::write(&at_limit, vec![1u8; limit]).expect("write");
        std::fs::write(&over, vec![1u8; limit + 1]).expect("write");

        let (name, data) =
            read_attachment(&at_limit, MAX_ATTACHMENT_BYTES).expect("exactly 5 MiB is allowed");
        assert_eq!(name, "exact.bin");
        assert_eq!(data.len(), limit);
        assert!(matches!(
            read_attachment(&over, MAX_ATTACHMENT_BYTES),
            Err(CliError::Usage(_))
        ));
    }

    #[test]
    fn read_attachment_enforces_the_encrypted_limit() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("sealed.bin");
        let limit = attachment_limit(true);
        let len = usize::try_from(limit + 1).expect("fits usize");
        std::fs::write(&path, vec![1u8; len]).expect("write");
        let expected = format!("attachment sealed.bin is larger than {limit} bytes");
        assert!(matches!(
            read_attachment(&path, limit),
            Err(CliError::Usage(m)) if m == expected
        ));
    }
}
