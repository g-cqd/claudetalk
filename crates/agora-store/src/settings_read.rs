//! The one reader of the stored work settings (`work_settings` row 1). Every read path goes through [`settings_in`], so a
//! stored value means the same thing wherever it is used.

use crate::StoreError;
use agora_sched::settings::WorkSettings;
use rusqlite::{Connection, OptionalExtension};

/// The stored work settings, or the defaults when no row exists. A stored claim TTL below 60 s is read as 60 s; the
/// row itself is left as it is, so a later settings write persists a valid value.
pub(crate) fn settings_in(conn: &Connection) -> Result<WorkSettings, StoreError> {
    let raw: Option<String> = conn
        .query_row("SELECT json FROM work_settings WHERE id=1", [], |r| {
            r.get(0)
        })
        .optional()?;
    let Some(json) = raw else {
        return Ok(WorkSettings::default());
    };
    let mut settings: WorkSettings = serde_json::from_str(&json)
        .map_err(|_| StoreError::Invalid("corrupt work settings in database"))?;
    let stored = settings.claim_ttl_s;
    if settings.normalize_stored() {
        tracing::warn!(
            stored = stored,
            "work settings: claim_ttl_s below 60 s clamped to 60 s on read"
        );
    }
    Ok(settings)
}
