//! The per-session delivery cursor shared between the channel push (this crate) and the Claude
//! Code hooks (`agora-hooks`): both read and advance the same `agora_client::session_state`
//! cursor, keyed by the Claude Code session id, so a message pushed on the channel is not
//! re-injected by the hooks of the same session, and vice versa. Nothing here acks a message.

use agora_client::session_state;
use serde_json::Value;
use std::ffi::OsString;
use std::path::Path;

/// Env var Claude Code sets for the processes it spawns (the MCP server included).
pub const ENV: &str = "CLAUDE_CODE_SESSION_ID";
/// Longest session id accepted from the env, in bytes.
const MAX_ID_BYTES: usize = 256;

/// The Claude Code session id from this process's env (Claude Code sets it for child processes):
/// non-empty, <= 256 bytes, else None. Read once at push-task start.
pub fn from_env() -> Option<String> {
    parse_session(std::env::var_os(ENV))
}

/// The session id held by a raw env value: UTF-8, non-empty and at most `MAX_ID_BYTES` bytes.
fn parse_session(raw: Option<OsString>) -> Option<String> {
    let id = raw?.into_string().ok()?;
    (!id.is_empty() && id.len() <= MAX_ID_BYTES).then_some(id)
}

/// Remove from `msgs` every message whose integer `seq` <= the session's hook cursor
/// (unlocked `session_state::load`). Messages without an integer `seq` are kept.
pub fn drop_delivered(home: &Path, session_id: &str, msgs: &mut Vec<Value>) {
    let cursor = session_state::load(home, session_id).cursor;
    msgs.retain(|m| crate::push::seq_of(m).is_none_or(|seq| seq > cursor));
}

/// After a successful channel push: `session_state::try_advance(home, sid, max_seq)`;
/// errors are ignored (a possible duplicate is preferred over a loss).
pub fn mark_delivered(home: &Path, session_id: &str, max_seq: i64) {
    if let Err(e) = session_state::try_advance(home, session_id, max_seq) {
        tracing::debug!(error = %e, "session cursor not advanced");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;

    const SID: &str = "11111111-2222-3333-4444-555555555555";

    fn msg(seq: i64) -> Value {
        serde_json::json!({"seq": seq, "channel": "general", "text": "hi"})
    }

    #[test]
    fn parse_session_accepts_a_plain_id() {
        assert_eq!(
            parse_session(Some(OsString::from(SID))),
            Some(SID.to_owned())
        );
    }

    #[test]
    fn parse_session_rejects_unset_empty_and_non_utf8_values() {
        assert_eq!(parse_session(None), None);
        assert_eq!(parse_session(Some(OsString::new())), None);
        let bytes = OsString::from_vec(vec![b'a', 0xff]);
        assert_eq!(parse_session(Some(bytes)), None);
    }

    #[test]
    fn parse_session_bounds_the_length_in_bytes() {
        let longest = "x".repeat(MAX_ID_BYTES);
        assert_eq!(parse_session(Some(OsString::from(&longest))), Some(longest));
        let too_long = "x".repeat(MAX_ID_BYTES + 1);
        assert_eq!(parse_session(Some(OsString::from(too_long))), None);
        // Two bytes per char: 129 chars are 258 bytes, over the limit though under 256 chars.
        let wide = "é".repeat(129);
        assert_eq!(parse_session(Some(OsString::from(wide))), None);
    }

    #[test]
    fn drop_delivered_keeps_what_is_past_the_cursor_and_seqless_messages() {
        let home = tempfile::tempdir().expect("tempdir");
        assert!(session_state::try_advance(home.path(), SID, 5).expect("advance"));
        let mut msgs = vec![
            msg(4),
            msg(5),
            msg(6),
            msg(7),
            serde_json::json!({"text": "no seq"}),
        ];
        drop_delivered(home.path(), SID, &mut msgs);
        let kept: Vec<Option<i64>> = msgs.iter().map(crate::push::seq_of).collect();
        assert_eq!(kept, [Some(6), Some(7), None]);
    }

    #[test]
    fn drop_delivered_keeps_everything_without_a_cursor() {
        let home = tempfile::tempdir().expect("tempdir");
        let mut msgs = vec![msg(1), msg(2)];
        drop_delivered(home.path(), SID, &mut msgs);
        assert_eq!(msgs.len(), 2);
    }

    #[test]
    fn mark_delivered_advances_the_cursor_and_never_moves_it_back() {
        let home = tempfile::tempdir().expect("tempdir");
        mark_delivered(home.path(), SID, 5);
        assert_eq!(session_state::load(home.path(), SID).cursor, 5);
        mark_delivered(home.path(), SID, 3);
        assert_eq!(session_state::load(home.path(), SID).cursor, 5);
        mark_delivered(home.path(), SID, 9);
        assert_eq!(session_state::load(home.path(), SID).cursor, 9);
    }

    #[test]
    fn mark_delivered_with_a_missing_home_is_silent_and_creates_nothing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let missing = tmp.path().join("no-such-home");
        mark_delivered(&missing, SID, 3);
        assert!(!missing.exists(), "the home is never created");
    }
}
