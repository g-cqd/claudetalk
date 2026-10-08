//! Tool handlers: chat, attach, read, react, fetch_file, inbox, inbox_wait, ack.

use super::args::*;
use super::{ToolCx, ToolError, ToolOutput, ToolResult};
use crate::framing;
use agora_client::Out;
use agora_client::api::chat as verbs;
use agora_client::api::chat::{
    AckParams, AttachmentInput, ChatParams, FetchParams, InboxParams, MAX_ATTACHMENT_BYTES,
    ReactParams, ReadParams,
};
use agora_client::ops::write_private_file;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};
use std::io::Read as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;
use std::time::Duration;
use tokio::time::Instant;

/// Longest `inbox_wait` in seconds.
const MAX_WAIT_SECS: u64 = 55;
/// Largest blob `fetch_file` returns inline as base64.
const INLINE_BLOB_MAX: usize = 1024 * 1024;

/// Post a message (optionally with attachments) to a channel via `api::chat::chat`. Each attachment has exactly one of `path` (stdio only) or `base64` (decoded size cap 5 MiB), else a usage error; over HTTP `path` is refused with usage "filesystem paths are disabled over HTTP; use base64". Json `{seq,id,channel}`; text = the api text.
pub async fn chat(cx: &ToolCx<'_>, a: ChatArgs) -> ToolResult {
    let attachments = attachment_inputs(cx, &a.attachments.unwrap_or_default()).await?;
    let out = verbs::chat(
        &cx.peer.ctx,
        ChatParams {
            channel: a.channel,
            text: a.text,
            thread_of: a.thread_of,
            attachments,
        },
    )
    .await?;
    Ok(plain(out))
}

/// Post one attachment (`name` + `path` or `base64`) with optional `text` (default empty) via `api::chat::chat`, same rules as `chat`.
pub async fn attach(cx: &ToolCx<'_>, a: AttachArgs) -> ToolResult {
    let arg = AttachmentArg {
        name: a.name,
        path: a.path,
        base64: a.base64,
    };
    let attachment =
        attachment_input(&arg, cx.engine.config.allow_paths, &cx.peer.ctx.paths.home).await?;
    let out = verbs::chat(
        &cx.peer.ctx,
        ChatParams {
            channel: a.channel,
            text: a.text,
            thread_of: a.thread_of,
            attachments: vec![attachment],
        },
    )
    .await?;
    Ok(plain(out))
}

/// Read a channel via `api::chat::read` (limit clamped to 1..=200). Message-bearing: text is the framed batch (`framing::frame_batch`), structured is `framing::structured_messages(api json)`.
pub async fn read(cx: &ToolCx<'_>, a: ReadArgs) -> ToolResult {
    let out = verbs::read(
        &cx.peer.ctx,
        ReadParams {
            channel: a.channel,
            after: a.after,
            limit: clamp_limit(a.limit),
        },
    )
    .await?;
    Ok(ToolOutput {
        text: message_text(messages(&out.json)),
        structured: framing::structured_messages(out.json),
    })
}

/// Add or remove a reaction via `api::chat::react` (`kind` message|file|card, default message).
pub async fn react(cx: &ToolCx<'_>, a: ReactArgs) -> ToolResult {
    check_kind(&a.kind)?;
    let out = verbs::react(
        &cx.peer.ctx,
        ReactParams {
            target: a.target,
            emoji: a.emoji,
            kind: a.kind,
            remove: a.remove,
        },
    )
    .await?;
    Ok(plain(out))
}

/// Fetch an attachment blob via `api::chat::fetch`. With `save_to` (stdio only): `write_private_file(path, bytes, overwrite=false)` and json `{hash,bytes,saved_to}`; else inline `{hash,bytes,base64}` when <= 1 MiB, else a usage error "blob is N bytes; too large to return inline (use save_to on a local stdio session)".
pub async fn fetch_file(cx: &ToolCx<'_>, a: FetchFileArgs) -> ToolResult {
    if a.save_to.is_some() {
        path_guard(cx.engine.config.allow_paths)?;
    }
    let fetched = verbs::fetch(
        &cx.peer.ctx,
        FetchParams {
            hash: a.hash,
            channel: a.channel,
            epoch: a.epoch,
        },
    )
    .await?;
    match a.save_to {
        Some(path) => {
            write_private_file(Path::new(&path), &fetched.bytes, false)?;
            let len = fetched.bytes.len();
            Ok(ToolOutput {
                structured: json!({"hash": fetched.hash, "bytes": len, "saved_to": path}),
                text: format!("saved {len} bytes to {path}"),
            })
        }
        None => inline_blob(&fetched.hash, &fetched.bytes),
    }
}

/// Unacked messages across your channels via `api::chat::inbox` without waiting (`after` cursor, limit clamped to 1..=200). Message-bearing: framed text and `structured_messages`.
pub async fn inbox(cx: &ToolCx<'_>, a: InboxArgs) -> ToolResult {
    let out = verbs::inbox(
        &cx.peer.ctx,
        InboxParams {
            wait: None,
            after: a.after,
            limit: clamp_limit(a.limit),
        },
    )
    .await?;
    Ok(inbox_output(out.json))
}

/// Block for new messages via `api::chat::inbox` with `wait = timeout_secs` (default 55, max 55; limit default 20 clamped to 1..=200). If the request has a progress token, send `Progress::report` every `config.progress_interval_ms` with strictly increasing progress (elapsed seconds) and total = timeout; `cx.cancel` drops the wait immediately (no further daemon work, no result). Message-bearing: framed text and `structured_messages`; json keeps `cursor` and `timed_out`.
pub async fn inbox_wait(cx: &ToolCx<'_>, a: InboxWaitArgs) -> ToolResult {
    let timeout = a.timeout_secs.min(MAX_WAIT_SECS);
    let fut = verbs::inbox(
        &cx.peer.ctx,
        InboxParams {
            wait: Some(timeout),
            after: a.cursor,
            limit: clamp_limit(a.limit),
        },
    );
    tokio::pin!(fut);

    let started = Instant::now();
    let mut ticks = match &cx.progress {
        Some(_) => {
            let period = Duration::from_millis(cx.engine.config.progress_interval_ms.max(1));
            let mut ticks = tokio::time::interval(period);
            // The first tick of an interval is immediate; progress starts one period in.
            ticks.tick().await;
            Some(ticks)
        }
        None => None,
    };
    let total = f64::from(u32::try_from(timeout).unwrap_or(u32::MAX));

    loop {
        tokio::select! {
            res = &mut fut => {
                let out = res?;
                return Ok(wait_output(out.json, timeout));
            }
            () = cx.cancel.cancelled() => {
                return Err(ToolError {
                    code: "cancelled".into(),
                    message: "request cancelled".into(),
                });
            }
            at = next_tick(&mut ticks) => {
                if let Some(progress) = &cx.progress {
                    let elapsed = at.duration_since(started).as_secs_f64();
                    progress.report(elapsed, Some(total), "waiting for messages");
                }
            }
        }
    }
}

/// Mark a channel read up to `seq` via `api::chat::ack`.
pub async fn ack(cx: &ToolCx<'_>, a: AckArgs) -> ToolResult {
    let out = verbs::ack(
        &cx.peer.ctx,
        AckParams {
            channel: a.channel,
            seq: a.seq,
        },
    )
    .await?;
    Ok(plain(out))
}

/// A tool result that is the api JSON and text unchanged.
fn plain(out: Out) -> ToolOutput {
    ToolOutput {
        structured: out.json,
        text: out.text,
    }
}

/// Output of `inbox`: the framed messages (or "no messages") and a final "cursor N" line.
fn inbox_output(json: Value) -> ToolOutput {
    let text = format!(
        "{}\ncursor {}",
        message_text(messages(&json)),
        cursor_of(&json)
    );
    ToolOutput {
        structured: framing::structured_messages(json),
        text,
    }
}

/// Output of `inbox_wait`: like `inbox`, but a timeout with no messages says so.
fn wait_output(json: Value, timeout: u64) -> ToolOutput {
    let timed_out = json
        .get("timed_out")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let text = if timed_out && messages(&json).is_empty() {
        format!(
            "no new messages after waiting {timeout} s; cursor {}",
            cursor_of(&json)
        )
    } else {
        format!(
            "{}\ncursor {}",
            message_text(messages(&json)),
            cursor_of(&json)
        )
    };
    ToolOutput {
        structured: framing::structured_messages(json),
        text,
    }
}

/// Resolves at each progress tick; pends forever when there is no progress token.
async fn next_tick(ticks: &mut Option<tokio::time::Interval>) -> Instant {
    match ticks {
        Some(ticks) => ticks.tick().await,
        None => std::future::pending().await,
    }
}

/// The `messages` array of an api JSON object (empty when absent).
fn messages(json: &Value) -> &[Value] {
    json.get("messages")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

/// Framed batch of the messages, or "no messages".
fn message_text(messages: &[Value]) -> String {
    if messages.is_empty() {
        "no messages".to_owned()
    } else {
        framing::frame_batch(messages)
    }
}

/// The `cursor` field as printed, "0" when absent.
fn cursor_of(json: &Value) -> String {
    json.get("cursor")
        .map_or_else(|| "0".to_owned(), Value::to_string)
}

/// `limit` clamped to 1..=200.
fn clamp_limit(limit: i64) -> i64 {
    limit.clamp(1, 200)
}

/// `react` kinds the daemon knows.
fn check_kind(kind: &str) -> Result<(), ToolError> {
    match kind {
        "message" | "file" | "card" => Ok(()),
        _ => Err(ToolError::usage("kind must be message, file or card")),
    }
}

/// `path` arguments exist only when the engine allows filesystem paths (never over HTTP).
fn path_guard(allow_paths: bool) -> Result<(), ToolError> {
    if allow_paths {
        Ok(())
    } else {
        Err(ToolError::usage(
            "filesystem paths are disabled over HTTP; use base64",
        ))
    }
}

/// The usage error for an attachment over the size cap.
fn too_large(name: &str) -> ToolError {
    ToolError::usage(format!(
        "attachment {name} is larger than {MAX_ATTACHMENT_BYTES} bytes"
    ))
}

/// The api inputs of the attachment arguments, in order; the first refused one fails the call.
async fn attachment_inputs(
    cx: &ToolCx<'_>,
    args: &[AttachmentArg],
) -> Result<Vec<AttachmentInput>, ToolError> {
    let allow_paths = cx.engine.config.allow_paths;
    let home = &cx.peer.ctx.paths.home;
    let mut inputs = Vec::with_capacity(args.len());
    for arg in args {
        inputs.push(attachment_input(arg, allow_paths, home).await?);
    }
    Ok(inputs)
}

/// One attachment argument as the api input: exactly one of `path` (allowed only when
/// `allow_paths`) or `base64`. Size caps are checked before any read or decode. A path is read
/// on a blocking thread, never on the async runtime.
async fn attachment_input(
    a: &AttachmentArg,
    allow_paths: bool,
    home: &Path,
) -> Result<AttachmentInput, ToolError> {
    let bytes = match (&a.path, &a.base64) {
        (Some(path), None) => {
            path_guard(allow_paths)?;
            let (name, path, home) = (a.name.clone(), path.clone(), home.to_path_buf());
            tokio::task::spawn_blocking(move || read_attachment(&name, &path, &home))
                .await
                .map_err(|_| ToolError {
                    code: "internal".into(),
                    message: format!("attachment {}: read failed", a.name),
                })??
        }
        (None, Some(encoded)) => decode_bounded(&a.name, encoded)?,
        _ => {
            return Err(ToolError::usage(format!(
                "attachment {}: give exactly one of path or base64",
                a.name
            )));
        }
    };
    Ok(AttachmentInput {
        name: a.name.clone(),
        bytes,
    })
}

/// Read a local file for an attachment. The path is canonicalized and refused when it lies inside
/// the canonical agora `home`. The file is opened `O_NONBLOCK`, so a FIFO or device cannot block
/// the open, and only a regular file (checked on the open descriptor) is read. At most one byte
/// past the cap is read, so an oversize file is detected without being read whole.
fn read_attachment(name: &str, path: &str, home: &Path) -> Result<Vec<u8>, ToolError> {
    let io = |e: std::io::Error| ToolError::usage(format!("attachment {name}: {path}: {e}"));
    let canonical = std::fs::canonicalize(path).map_err(io)?;
    let home = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    if canonical.starts_with(&home) {
        return Err(ToolError::usage(
            "refusing to attach files from the agora home",
        ));
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&canonical)
        .map_err(io)?;
    if !file.metadata().map_err(io)?.is_file() {
        return Err(ToolError::usage(format!(
            "attachment {name}: {path} is not a regular file"
        )));
    }
    let mut bytes = Vec::new();
    file.take(MAX_ATTACHMENT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(io)?;
    if bytes.len() > usize_cap() {
        return Err(too_large(name));
    }
    Ok(bytes)
}

/// Decode standard base64. The encoded length is checked first: a longer string cannot
/// decode within the cap, so it is refused without decoding.
fn decode_bounded(name: &str, encoded: &str) -> Result<Vec<u8>, ToolError> {
    if encoded.len() / 4 * 3 > usize_cap().saturating_add(2) {
        return Err(too_large(name));
    }
    STANDARD
        .decode(encoded)
        .map_err(|_| ToolError::usage(format!("attachment {name}: base64 is not valid")))
}

/// The attachment cap as a `usize` (5 MiB fits on every supported target).
fn usize_cap() -> usize {
    usize::try_from(MAX_ATTACHMENT_BYTES).unwrap_or(usize::MAX)
}

/// Inline output of a blob: base64 when at most 1 MiB, else a usage error naming the size.
fn inline_blob(hash: &str, bytes: &[u8]) -> ToolResult {
    let len = bytes.len();
    if len > INLINE_BLOB_MAX {
        return Err(ToolError::usage(format!(
            "blob is {len} bytes; too large to return inline (use save_to on a local stdio session)"
        )));
    }
    Ok(ToolOutput {
        structured: json!({"hash": hash, "bytes": len, "base64": STANDARD.encode(bytes)}),
        text: format!("{len} bytes (base64 in structured output)"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arg(name: &str, path: Option<&str>, base64: Option<&str>) -> AttachmentArg {
        AttachmentArg {
            name: name.into(),
            path: path.map(str::to_owned),
            base64: base64.map(str::to_owned),
        }
    }

    /// A home directory that no test file lives in.
    fn home() -> std::path::PathBuf {
        std::path::PathBuf::from("/nonexistent-agora-home")
    }

    fn usage_message(r: Result<AttachmentInput, ToolError>) -> String {
        let e = r.expect_err("usage error");
        assert_eq!(e.code, "usage", "{}", e.message);
        e.message
    }

    #[tokio::test]
    async fn attachment_needs_exactly_one_of_path_or_base64() {
        let both = arg("a.txt", Some("/tmp/x"), Some("aGk="));
        assert_eq!(
            usage_message(attachment_input(&both, true, &home()).await),
            "attachment a.txt: give exactly one of path or base64"
        );
        let neither = arg("a.txt", None, None);
        assert_eq!(
            usage_message(attachment_input(&neither, true, &home()).await),
            "attachment a.txt: give exactly one of path or base64"
        );
    }

    #[tokio::test]
    async fn base64_attachment_decodes_to_its_bytes() {
        let input = attachment_input(&arg("hi.txt", None, Some("aGk=")), false, &home())
            .await
            .expect("decodes");
        assert_eq!(input.name, "hi.txt");
        assert_eq!(input.bytes, b"hi");
    }

    #[tokio::test]
    async fn invalid_base64_is_a_usage_error_naming_the_attachment() {
        assert_eq!(
            usage_message(
                attachment_input(&arg("bad.bin", None, Some("!!!!")), true, &home()).await
            ),
            "attachment bad.bin: base64 is not valid"
        );
    }

    #[tokio::test]
    async fn oversize_base64_is_refused_before_decoding() {
        // Not valid base64, so only the length precheck can produce the "larger" message.
        let encoded = "!".repeat(usize_cap() / 3 * 4 + 8);
        let message = usage_message(
            attachment_input(&arg("big.bin", None, Some(&encoded)), true, &home()).await,
        );
        assert_eq!(
            message,
            format!("attachment big.bin is larger than {MAX_ATTACHMENT_BYTES} bytes")
        );
    }

    #[tokio::test]
    async fn path_is_refused_when_paths_are_disabled() {
        assert_eq!(
            usage_message(
                attachment_input(&arg("a.txt", Some("/nonexistent/a"), None), false, &home(),)
                    .await
            ),
            "filesystem paths are disabled over HTTP; use base64"
        );
    }

    #[tokio::test]
    async fn path_attachment_reads_the_file_when_allowed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("note.txt");
        std::fs::write(&file, b"hello").expect("write");
        let p = file.to_str().expect("utf8 path");
        let input = attachment_input(&arg("note.txt", Some(p), None), true, &home())
            .await
            .expect("reads");
        assert_eq!(input.bytes, b"hello");
    }

    #[tokio::test]
    async fn path_attachment_names_the_attachment_when_unreadable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("missing.bin");
        let p = missing.to_str().expect("utf8 path");
        let message = usage_message(
            attachment_input(&arg("missing.bin", Some(p), None), true, &home()).await,
        );
        assert!(message.starts_with("attachment missing.bin: "), "{message}");
        assert!(message.contains(p), "{message}");
    }

    #[tokio::test]
    async fn oversize_path_attachment_is_refused_and_named() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("big.bin");
        let big = vec![0u8; usize_cap() + 1];
        std::fs::write(&file, &big).expect("write");
        let p = file.to_str().expect("utf8 path");
        assert_eq!(
            usage_message(attachment_input(&arg("big.bin", Some(p), None), true, &home()).await),
            format!("attachment big.bin is larger than {MAX_ATTACHMENT_BYTES} bytes")
        );
    }

    #[tokio::test]
    async fn a_directory_is_refused_as_not_a_regular_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().to_str().expect("utf8 path");
        let message =
            usage_message(attachment_input(&arg("dir", Some(p), None), true, &home()).await);
        assert!(message.ends_with("is not a regular file"), "{message}");
    }

    #[test]
    fn limits_clamp_to_one_through_two_hundred() {
        assert_eq!(clamp_limit(0), 1);
        assert_eq!(clamp_limit(-5), 1);
        assert_eq!(clamp_limit(50), 50);
        assert_eq!(clamp_limit(500), 200);
    }

    #[test]
    fn react_kind_accepts_only_the_three_kinds() {
        for ok in ["message", "file", "card"] {
            assert!(check_kind(ok).is_ok(), "{ok}");
        }
        assert_eq!(check_kind("Message").expect_err("refused").code, "usage");
        assert_eq!(check_kind("").expect_err("refused").code, "usage");
    }

    #[test]
    fn inline_blob_returns_base64_up_to_one_mib() {
        let out = inline_blob("h", b"hi").expect("inline");
        assert_eq!(out.structured["bytes"], 2);
        assert_eq!(out.structured["base64"], "aGk=");
        assert_eq!(out.text, "2 bytes (base64 in structured output)");
        assert!(inline_blob("h", &vec![0u8; INLINE_BLOB_MAX]).is_ok());
    }

    #[test]
    fn inline_blob_over_one_mib_asks_for_save_to() {
        let len = INLINE_BLOB_MAX + 1;
        let e = inline_blob("h", &vec![0u8; len]).expect_err("too big");
        assert_eq!(e.code, "usage");
        assert_eq!(
            e.message,
            format!(
                "blob is {len} bytes; too large to return inline (use save_to on a local stdio session)"
            )
        );
    }

    #[test]
    fn inbox_text_says_no_messages_then_the_cursor() {
        let out = inbox_output(json!({"messages": [], "cursor": 7, "timed_out": false}));
        assert_eq!(out.text, "no messages\ncursor 7");
        assert_eq!(out.structured["notice"], framing::UNTRUSTED_NOTICE);
    }

    #[test]
    fn wait_text_reports_the_timeout_only_without_messages() {
        let timed = wait_output(json!({"messages": [], "cursor": 3, "timed_out": true}), 12);
        assert_eq!(timed.text, "no new messages after waiting 12 s; cursor 3");
        assert_eq!(timed.structured["timed_out"], true);
        let got = wait_output(
            json!({"messages": [{"seq": 4, "text": "hi"}], "cursor": 4, "timed_out": false}),
            12,
        );
        assert!(got.text.ends_with("\ncursor 4"), "{}", got.text);
        assert!(got.text.contains("<untrusted-message"), "{}", got.text);
    }
}
