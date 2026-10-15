//! chat, read, inbox, wait, ack, react, fetch: clap args -> `agora_client::api::chat` -> Out.

use crate::{
    AckArgs, ChatArgs, CliError, Ctx, FetchArgs, InboxArgs, Out, ReactArgs, ReadArgs, WaitArgs,
};
use agora_client::api::chat::{
    self as api, AckParams, AttachmentInput, ChatParams, FetchParams, InboxParams,
    MAX_ATTACHMENT_BYTES, ReactParams, ReadParams, WaitParams,
};
use agora_client::ops::write_private_file;
use std::io::Read;
use std::path::Path;

pub async fn chat(ctx: &Ctx, a: ChatArgs) -> Result<Out, CliError> {
    let mut attachments = Vec::with_capacity(a.attach.len());
    for path in &a.attach {
        // The unencrypted cap bounds the read; `api::chat` applies the exact limit of the channel.
        let (name, bytes) = read_attachment(path, MAX_ATTACHMENT_BYTES)?;
        attachments.push(AttachmentInput { name, bytes });
    }
    api::chat(
        ctx,
        ChatParams {
            channel: a.channel,
            text: a.text,
            thread_of: a.thread_of,
            attachments,
        },
    )
    .await
}

pub async fn read(ctx: &Ctx, a: ReadArgs) -> Result<Out, CliError> {
    api::read(
        ctx,
        ReadParams {
            channel: a.channel,
            after: a.after,
            limit: a.limit,
        },
    )
    .await
}

pub async fn inbox(ctx: &Ctx, a: InboxArgs) -> Result<Out, CliError> {
    api::inbox(
        ctx,
        InboxParams {
            wait: a.wait,
            after: a.after,
            limit: a.limit,
        },
    )
    .await
}

/// `agora wait`: block until unacked messages arrive, without a timeout. The
/// command for agents that must not poll: it returns the moment a message
/// arrives, with the cursor for the next call.
pub async fn wait(ctx: &Ctx, a: WaitArgs) -> Result<Out, CliError> {
    api::wait(
        ctx,
        WaitParams {
            after: a.after,
            limit: a.limit,
        },
    )
    .await
}

pub async fn ack(ctx: &Ctx, a: AckArgs) -> Result<Out, CliError> {
    api::ack(
        ctx,
        AckParams {
            channel: a.channel,
            seq: a.seq,
        },
    )
    .await
}

pub async fn react(ctx: &Ctx, a: ReactArgs) -> Result<Out, CliError> {
    api::react(
        ctx,
        ReactParams {
            target: a.target,
            emoji: a.emoji,
            kind: a.kind,
            remove: a.remove,
        },
    )
    .await
}

pub async fn fetch(ctx: &Ctx, a: FetchArgs) -> Result<Out, CliError> {
    let FetchArgs {
        hash,
        out,
        channel,
        epoch,
    } = a;
    let fetched = api::fetch(
        ctx,
        FetchParams {
            hash,
            channel,
            epoch,
        },
    )
    .await?;
    write_private_file(&out, &fetched.bytes, true)?;
    let mut json = fetched.json;
    json["out"] = serde_json::json!(out.display().to_string());
    Ok(Out {
        json,
        text: format!("wrote {} bytes to {}", fetched.bytes.len(), out.display()),
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_failures_on_attach_name_the_path() {
        let missing = Path::new("/nonexistent-agora-dir/file.bin");
        let err = read_attachment(missing, 10).expect_err("missing file");
        assert!(matches!(err, CliError::Usage(m) if m.contains("file.bin:")));
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
        let expected = format!("attachment over.bin is larger than {MAX_ATTACHMENT_BYTES} bytes");
        assert!(matches!(
            read_attachment(&over, MAX_ATTACHMENT_BYTES),
            Err(CliError::Usage(m)) if m == expected
        ));
    }
}
