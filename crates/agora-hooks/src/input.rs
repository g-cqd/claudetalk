//! Reading the hook stdin object, defensively: it is untrusted input.

use std::io::Read;

use crate::HookError;
use crate::schema::HookInput;

/// Largest stdin a hook reads.
pub const MAX_STDIN_BYTES: usize = 256 * 1024;

/// Read at most `MAX_STDIN_BYTES + 1` bytes from `r`; more than `MAX_STDIN_BYTES` ->
/// `Err(HookError::Input("stdin too large"))`; then `parse_input`. Read errors -> `Io`.
pub fn read_input(r: &mut dyn std::io::Read) -> Result<HookInput, HookError> {
    let limit = u64::try_from(MAX_STDIN_BYTES + 1).unwrap_or(u64::MAX);
    let mut buf = Vec::new();
    r.take(limit).read_to_end(&mut buf)?;
    if buf.len() > MAX_STDIN_BYTES {
        return Err(HookError::Input("stdin too large".into()));
    }
    parse_input(&buf)
}

/// Empty or whitespace-only input, invalid UTF-8, invalid JSON, or a JSON value that is not an
/// object -> `Err(HookError::Input(..))`. Otherwise the object's known fields (a field of the
/// wrong type is an error too).
pub fn parse_input(bytes: &[u8]) -> Result<HookInput, HookError> {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Err(HookError::Input("empty stdin".into()));
    }
    let text =
        std::str::from_utf8(bytes).map_err(|_| HookError::Input("stdin is not UTF-8".into()))?;
    let value: serde_json::Value = serde_json::from_str(text)?;
    if !value.is_object() {
        return Err(HookError::Input("stdin is not a JSON object".into()));
    }
    Ok(serde_json::from_value(value)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    const SESSION_START: &str = r#"{"session_id":"e773b56f-0c1d-4a2b-9f3e-6a7b8c9d0e1f","transcript_path":"/Users/gc/.claude/projects/-private-tmp-agora-m4-probe-work/e773b56f-0c1d-4a2b-9f3e-6a7b8c9d0e1f.jsonl","cwd":"/private/tmp/agora-m4/probe/work","hook_event_name":"SessionStart","source":"startup"}"#;

    /// A JSON object of exactly `len` bytes: spaces pad the gap before the closing brace.
    fn padded(len: usize) -> Vec<u8> {
        let head = r#"{"hook_event_name":"Stop""#;
        let pad = len - head.len() - 1;
        format!("{head}{}}}", " ".repeat(pad)).into_bytes()
    }

    struct Broken;

    impl Read for Broken {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("broken pipe"))
        }
    }

    #[test]
    fn valid_session_start_parses() {
        let parsed = parse_input(SESSION_START.as_bytes()).expect("valid sample");
        assert_eq!(parsed.hook_event_name, "SessionStart");
        assert_eq!(parsed.source.as_deref(), Some("startup"));
        assert_eq!(
            parsed.cwd.as_deref(),
            Some("/private/tmp/agora-m4/probe/work")
        );
        assert_eq!(parsed.session_id, "e773b56f-0c1d-4a2b-9f3e-6a7b8c9d0e1f");
    }

    #[test]
    fn exact_max_bytes_accepted() {
        let exact = padded(MAX_STDIN_BYTES);
        assert_eq!(exact.len(), MAX_STDIN_BYTES);
        let parsed = read_input(&mut exact.as_slice()).expect("exact max is accepted");
        assert_eq!(parsed.hook_event_name, "Stop");
    }

    #[test]
    fn one_byte_over_max_rejected() {
        let over = padded(MAX_STDIN_BYTES + 1);
        assert!(matches!(
            read_input(&mut over.as_slice()),
            Err(HookError::Input(m)) if m == "stdin too large"
        ));
    }

    #[test]
    fn ten_mib_rejected_without_reading_it_all() {
        let mut big = std::io::repeat(b' ').take(10 << 20);
        assert!(read_input(&mut big).is_err());
    }

    #[test]
    fn empty_and_whitespace_rejected() {
        for bytes in [&b""[..], &b" \t\r\n"[..]] {
            assert!(matches!(
                parse_input(bytes),
                Err(HookError::Input(m)) if m == "empty stdin"
            ));
        }
    }

    #[test]
    fn invalid_utf8_rejected() {
        assert!(matches!(
            parse_input(&[0xff, 0xfe]),
            Err(HookError::Input(m)) if m == "stdin is not UTF-8"
        ));
    }

    #[test]
    fn non_object_json_rejected() {
        for text in ["[]", "\"str\"", "null", "42"] {
            assert!(matches!(
                parse_input(text.as_bytes()),
                Err(HookError::Input(m)) if m == "stdin is not a JSON object"
            ));
        }
    }

    #[test]
    fn truncated_json_rejected() {
        assert!(matches!(
            parse_input(br#"{"session_id":"abc""#),
            Err(HookError::Input(_) | HookError::Json(_))
        ));
    }

    #[test]
    fn wrong_field_type_is_json_error() {
        assert!(matches!(
            parse_input(br#"{"session_id":5}"#),
            Err(HookError::Json(_))
        ));
    }

    #[test]
    fn read_error_is_io() {
        assert!(matches!(read_input(&mut Broken), Err(HookError::Io(_))));
    }
}
