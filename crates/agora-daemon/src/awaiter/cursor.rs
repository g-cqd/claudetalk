//! The opaque wait cursor: the base64 (`agora_proto::b64`) of compact JSON holding the accumulated milliseconds, the log
//! offset and carry (at most `watch::MAX_CARRY` bytes), the snapshot ids of an All wait (at most `MAX_TARGETS`), and
//! the resolved seq of a Notices wait. A cursor that breaks any of these bounds is refused as invalid.

use super::rules::within_targets;
use crate::DaemonError;
use crate::watch::MAX_CARRY;
use agora_proto::{b64, unb64};
use agora_store::StoreError;
use serde::{Deserialize, Serialize};

/// The resume state carried in the cursor.
#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
pub(super) struct Cursor {
    #[serde(default)]
    pub(super) elapsed_ms: i64,
    #[serde(default)]
    pub(super) offset: u64,
    #[serde(default)]
    pub(super) carry: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) ids: Vec<String>,
    /// The seq a Notices wait starts after, once it was resolved (a negative `after` resolves to the latest seq).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) notice_after: Option<i64>,
}

/// Cursor text. Serializing this plain struct cannot fail; the fallback is the empty object.
pub(super) fn encode_cursor(cursor: &Cursor) -> String {
    let json = serde_json::to_vec(cursor).unwrap_or_else(|_| b"{}".to_vec());
    b64(&json)
}

/// Decodes a cursor; `None` when the request had none. Anything else that is not a cursor, or a cursor over a bound,
/// is refused.
pub(super) fn decode_cursor(text: Option<&str>) -> Result<Option<Cursor>, DaemonError> {
    text.map(|t| {
        let bytes = unb64(t, "cursor").map_err(|_| invalid_cursor())?;
        let cursor: Cursor = serde_json::from_slice(&bytes).map_err(|_| invalid_cursor())?;
        if cursor.carry.len() > MAX_CARRY {
            return Err(invalid_cursor());
        }
        within_targets(cursor.ids.len())?;
        Ok(cursor)
    })
    .transpose()
}

fn invalid_cursor() -> DaemonError {
    DaemonError::Store(StoreError::Invalid("invalid wait cursor"))
}

/// Milliseconds as seconds to three decimals; negative input counts as zero.
pub(super) fn seconds(ms: i64) -> f64 {
    let s = ms.max(0) as f64 / 1000.0;
    (s * 1000.0).round() / 1000.0
}
