//! The `GET /event` stream of opencode: SSE framing and the busy state an event sets.

use super::OpencodeHttp;
use reqwest::Response;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// Largest SSE line or event payload accepted before the stream is dropped.
const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// The busy state an event sets for `session_id`: `session.status` (busy unless the status type is
/// `idle`) or `session.idle`. `None` for other events and for other sessions.
pub(super) fn busy_change(event: &Value, session_id: &str) -> Option<bool> {
    let props = event.get("properties")?;
    if props.get("sessionID")?.as_str()? != session_id {
        return None;
    }
    match event.get("type")?.as_str()? {
        "session.status" => Some(props.pointer("/status/type")?.as_str()? != "idle"),
        "session.idle" => Some(false),
        _ => None,
    }
}

/// Incremental `text/event-stream` parser: bytes in, the `data` payload of each completed event
/// out. Lines may split across chunks; a blank line ends an event; comments and other fields are
/// ignored.
#[derive(Default)]
pub(super) struct SseFrames {
    /// The bytes of the line still open.
    partial: Vec<u8>,
    /// The `data` lines of the event being read, joined by `\n`.
    data: Option<String>,
}

impl SseFrames {
    /// Appends `bytes` and returns the payloads of the events they complete. Only the new bytes
    /// are scanned for newlines, so a long line split over many chunks costs linear time.
    pub(super) fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>, String> {
        let mut buf = std::mem::take(&mut self.partial);
        let mut scan = buf.len();
        buf.extend_from_slice(bytes);
        let mut events = Vec::new();
        let mut start = 0;
        while let Some(offset) = buf[scan..].iter().position(|&b| b == b'\n') {
            let end = scan + offset;
            let line = String::from_utf8_lossy(&buf[start..end]);
            self.line(line.trim_end_matches('\r'), &mut events)?;
            start = end + 1;
            scan = start;
        }
        buf.drain(..start);
        if buf.len() > MAX_FRAME_BYTES {
            return Err("event line exceeds 8 MiB".into());
        }
        self.partial = buf;
        Ok(events)
    }

    fn line(&mut self, line: &str, events: &mut Vec<String>) -> Result<(), String> {
        if line.is_empty() {
            if let Some(data) = self.data.take() {
                events.push(data);
            }
            return Ok(());
        }
        // Comment lines start with ':'; `event`, `id` and `retry` are not used.
        let Some(value) = line.strip_prefix("data:") else {
            return Ok(());
        };
        let value = value.strip_prefix(' ').unwrap_or(value);
        match &mut self.data {
            Some(data) => {
                data.push('\n');
                data.push_str(value);
            }
            None => self.data = Some(value.to_owned()),
        }
        if self
            .data
            .as_ref()
            .is_some_and(|d| d.len() > MAX_FRAME_BYTES)
        {
            return Err("event data exceeds 8 MiB".into());
        }
        Ok(())
    }
}

/// How one event stream ended.
pub(super) enum StreamEnd {
    /// `stop` fired.
    Stopped,
    /// The stream closed or broke; the reason is for the log.
    Closed(String),
}

/// Reads SSE frames from `resp` until it ends or `stop` fires, applying the busy changes of
/// events for `session_id`.
pub(super) async fn read_events(
    resp: &mut Response,
    session_id: &str,
    busy: &watch::Sender<bool>,
    stop: &CancellationToken,
) -> StreamEnd {
    let mut frames = SseFrames::default();
    loop {
        let chunk = tokio::select! {
            _ = stop.cancelled() => return StreamEnd::Stopped,
            next = resp.chunk() => next,
        };
        let bytes = match chunk {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return StreamEnd::Closed("stream closed by the server".into()),
            Err(e) => return StreamEnd::Closed(format!("stream read failed: {}", e.without_url())),
        };
        let payloads = match frames.push(&bytes) {
            Ok(payloads) => payloads,
            Err(why) => return StreamEnd::Closed(why),
        };
        for payload in payloads {
            match serde_json::from_str::<Value>(&payload) {
                Ok(event) => {
                    if let Some(state) = busy_change(&event, session_id) {
                        busy.send_replace(state);
                    }
                }
                Err(e) => {
                    tracing::debug!(error = %e, "skipping an opencode event that is not JSON")
                }
            }
        }
    }
}

/// First reconnect delay of the event stream; doubled per failure up to `BACKOFF_MAX`.
const BACKOFF_FIRST: Duration = Duration::from_secs(1);
/// Longest delay between event-stream reconnects.
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Keeps `busy` current from `GET /event` for `session_id` until `stop`, reconnecting with backoff.
pub(super) async fn track(
    client: OpencodeHttp,
    session_id: String,
    busy: Arc<watch::Sender<bool>>,
    stop: CancellationToken,
) {
    let mut backoff = BACKOFF_FIRST;
    loop {
        // A server that never answers `GET /event` must not hold up shutdown.
        let opened = tokio::select! {
            _ = stop.cancelled() => return,
            opened = client.open_events() => opened,
        };
        let reason = match opened {
            Ok(mut resp) => {
                // The stream replays no status on connect, so the state is unknown until it speaks.
                // Assume idle: a prompt sent while the agent is busy is queued by the server, while
                // a missed idle would hold every later delivery.
                busy.send_replace(false);
                backoff = BACKOFF_FIRST;
                match read_events(&mut resp, &session_id, &busy, &stop).await {
                    StreamEnd::Stopped => return,
                    StreamEnd::Closed(reason) => reason,
                }
            }
            Err(e) => e.to_string(),
        };
        tracing::warn!(reason = %reason, "opencode event stream ended; reconnecting");
        tokio::select! {
            _ = stop.cancelled() => return,
            _ = tokio::time::sleep(backoff) => {}
        }
        backoff = next_backoff(backoff);
    }
}

/// The retry delay after `d`: doubled, at most `BACKOFF_MAX`.
fn next_backoff(d: Duration) -> Duration {
    d.saturating_mul(2).min(BACKOFF_MAX)
}
