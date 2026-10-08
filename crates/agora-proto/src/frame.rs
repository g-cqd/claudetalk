//! Length-prefixed JSON framing: u32 big-endian payload length, then the payload.

use crate::{MAX_FRAME, ProtoError};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Read one frame. `Ok(None)` on a clean EOF before the first length byte; EOF
/// inside the length or payload -> `Io(UnexpectedEof)`. A declared length above
/// `max` -> `FrameTooLarge(len)` WITHOUT reading the payload (the caller must then
/// drop the connection: the stream cannot be resynchronised). A zero-length frame
/// is returned as `Some(vec![])`.
///
/// The payload buffer grows with the bytes that actually arrive, so a peer that
/// declares a large length and sends little costs only what it sent.
///
/// Not cancel-safe: dropping the future after the first length byte has been read
/// discards the bytes already consumed, and the stream cannot be resynchronised.
/// Do not race it in `select!` or put it under a timeout unless the connection is
/// dropped on cancellation.
pub async fn read_frame<R: AsyncRead + Unpin>(
    r: &mut R,
    max: usize,
) -> Result<Option<Vec<u8>>, ProtoError> {
    let mut header = [0u8; 4];
    // A zero-byte read on the first length byte is the only clean end of stream.
    if r.read(&mut header[..1]).await? == 0 {
        return Ok(None);
    }
    // Any EOF from here on is a truncated frame and surfaces as UnexpectedEof.
    r.read_exact(&mut header[1..]).await?;

    let declared = u32::from_be_bytes(header);
    let len = usize::try_from(declared).unwrap_or(usize::MAX);
    if len > max {
        return Err(ProtoError::FrameTooLarge(len));
    }

    // Read through `take` rather than pre-allocating `len` bytes: the buffer grows
    // with the data, so a declared 8 MiB frame that sends 3 bytes costs 3 bytes.
    let mut payload = Vec::new();
    let read = r
        .take(u64::from(declared))
        .read_to_end(&mut payload)
        .await?;
    if read != len {
        return Err(ProtoError::Io(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "frame payload ended before the declared length",
        )));
    }
    Ok(Some(payload))
}

/// Write one frame and flush. Payload above MAX_FRAME -> `FrameTooLarge` and nothing
/// is written.
pub async fn write_frame<W: AsyncWrite + Unpin>(
    w: &mut W,
    payload: &[u8],
) -> Result<(), ProtoError> {
    if payload.len() > MAX_FRAME {
        return Err(ProtoError::FrameTooLarge(payload.len()));
    }
    let len = u32::try_from(payload.len()).map_err(|_| ProtoError::FrameTooLarge(payload.len()))?;

    // One buffer so the header and payload go out in a single write.
    let mut buf = Vec::with_capacity(4 + payload.len());
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(payload);
    w.write_all(&buf).await?;
    w.flush().await?;
    Ok(())
}

/// `read_frame(r, MAX_FRAME)` then `serde_json::from_slice`.
pub async fn read_json<T: DeserializeOwned, R: AsyncRead + Unpin>(
    r: &mut R,
) -> Result<Option<T>, ProtoError> {
    match read_frame(r, MAX_FRAME).await? {
        None => Ok(None),
        Some(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
    }
}

/// `serde_json::to_vec`, then a payload above `MAX_FRAME` -> `FrameTooLarge` before
/// anything is written; otherwise the 4-byte header and the payload go out as two
/// writes (no combined copy), then a flush.
pub async fn write_json<T: Serialize, W: AsyncWrite + Unpin>(
    w: &mut W,
    value: &T,
) -> Result<(), ProtoError> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > MAX_FRAME {
        return Err(ProtoError::FrameTooLarge(bytes.len()));
    }
    let len = u32::try_from(bytes.len()).map_err(|_| ProtoError::FrameTooLarge(bytes.len()))?;
    w.write_all(&len.to_be_bytes()).await?;
    w.write_all(&bytes).await?;
    w.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Req, Request};
    use std::io::ErrorKind;
    use tokio::io::duplex;

    fn is_eof(err: &ProtoError) -> bool {
        matches!(err, ProtoError::Io(e) if e.kind() == ErrorKind::UnexpectedEof)
    }

    #[tokio::test]
    async fn round_trips_several_frames_through_a_small_pipe() {
        let frames: Vec<Vec<u8>> = vec![
            b"first".to_vec(),
            Vec::new(),
            vec![7u8; 200],
            b"last".to_vec(),
        ];
        // The pipe holds 64 bytes, so the 200-byte frame needs the reader to drain it.
        let (mut a, mut b) = duplex(64);
        let to_write = frames.clone();
        let writer = tokio::spawn(async move {
            for f in &to_write {
                write_frame(&mut a, f).await.expect("write frame");
            }
        });

        for expected in &frames {
            let got = read_frame(&mut b, MAX_FRAME).await.expect("read frame");
            assert_eq!(got.as_ref(), Some(expected));
        }
        writer.await.expect("writer task");
    }

    #[tokio::test]
    async fn clean_eof_before_first_byte_is_none() {
        let (a, mut b) = duplex(64);
        drop(a);
        let got = read_frame(&mut b, MAX_FRAME).await.expect("clean eof");
        assert_eq!(got, None);
    }

    #[tokio::test]
    async fn eof_inside_header_is_unexpected_eof() {
        let (mut a, mut b) = duplex(64);
        a.write_all(&[0, 0]).await.expect("write partial header");
        drop(a);
        let err = read_frame(&mut b, MAX_FRAME)
            .await
            .expect_err("truncated header");
        assert!(is_eof(&err), "got {err:?}");
    }

    #[tokio::test]
    async fn eof_inside_payload_is_unexpected_eof() {
        let (mut a, mut b) = duplex(64);
        // Declares 10 payload bytes, sends 3.
        a.write_all(&10u32.to_be_bytes())
            .await
            .expect("write header");
        a.write_all(b"abc").await.expect("write partial payload");
        drop(a);
        let err = read_frame(&mut b, MAX_FRAME)
            .await
            .expect_err("truncated payload");
        assert!(is_eof(&err), "got {err:?}");
    }

    #[tokio::test]
    async fn declared_length_over_max_is_rejected_without_reading_payload() {
        let (mut a, mut b) = duplex(64);
        // Header only: a reader that tried to fetch the payload would see EOF instead.
        a.write_all(&17u32.to_be_bytes())
            .await
            .expect("write header");
        drop(a);
        let err = read_frame(&mut b, 16).await.expect_err("over max");
        assert!(matches!(err, ProtoError::FrameTooLarge(17)), "got {err:?}");
    }

    #[tokio::test]
    async fn zero_length_frame_is_some_empty() {
        let (mut a, mut b) = duplex(64);
        write_frame(&mut a, &[]).await.expect("write empty frame");
        drop(a);
        let got = read_frame(&mut b, MAX_FRAME)
            .await
            .expect("read empty frame");
        assert_eq!(got, Some(Vec::new()));
    }

    #[tokio::test]
    async fn write_over_max_is_rejected_and_writes_nothing() {
        let (mut a, mut b) = duplex(64);
        let too_big = vec![0u8; MAX_FRAME + 1];
        let err = write_frame(&mut a, &too_big)
            .await
            .expect_err("payload over MAX_FRAME");
        assert!(
            matches!(err, ProtoError::FrameTooLarge(n) if n == MAX_FRAME + 1),
            "got {err:?}"
        );
        drop(a);
        // Nothing reached the pipe, so the reader sees a clean EOF.
        let got = read_frame(&mut b, MAX_FRAME)
            .await
            .expect("no bytes written");
        assert_eq!(got, None);
    }

    #[tokio::test]
    async fn json_round_trip_preserves_request() {
        let sent = Request {
            id: 7,
            req: Req::Ping,
        };
        let (mut a, mut b) = duplex(64);
        let to_send = sent.clone();
        let writer = tokio::spawn(async move {
            write_json(&mut a, &to_send).await.expect("write json");
        });
        let got: Option<Request> = read_json(&mut b).await.expect("read json");
        writer.await.expect("writer task");
        assert_eq!(got, Some(sent));
    }

    #[tokio::test]
    async fn large_declared_length_with_short_payload_is_unexpected_eof() {
        let (mut a, mut b) = duplex(64);
        // Declares 1 MiB (within MAX_FRAME) and sends 3 bytes: the reader must report
        // EOF after the bytes that arrived, not allocate and wait for the rest.
        a.write_all(&(1u32 << 20).to_be_bytes())
            .await
            .expect("write header");
        a.write_all(b"abc").await.expect("write partial payload");
        drop(a);
        let err = read_frame(&mut b, MAX_FRAME)
            .await
            .expect_err("truncated large payload");
        assert!(is_eof(&err), "got {err:?}");
    }

    #[tokio::test]
    async fn write_json_over_max_is_rejected_and_writes_nothing() {
        let (mut a, mut b) = duplex(64);
        // The JSON encoding of this string is MAX_FRAME + 2 bytes (the quotes).
        let too_big = "x".repeat(MAX_FRAME);
        let err = write_json(&mut a, &too_big)
            .await
            .expect_err("encoded value over MAX_FRAME");
        assert!(
            matches!(err, ProtoError::FrameTooLarge(n) if n == MAX_FRAME + 2),
            "got {err:?}"
        );
        drop(a);
        // Neither the header nor the payload reached the pipe.
        let got = read_frame(&mut b, MAX_FRAME)
            .await
            .expect("no bytes written");
        assert_eq!(got, None);
    }
}
