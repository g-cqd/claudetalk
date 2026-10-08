//! Validation of the optional card fields: the `card.create` arg, and the value checks
//! shared with `card.update`.

use super::{AFTER_MSG, SIZE_MSG, VALUE_MSG};
use crate::{StoreError, require_card};
use rusqlite::{Transaction, params};

/// Optional fields of a `card.create` arg, validated before anything is written.
pub(crate) struct NewCardFields {
    pub(crate) priority: i64,
    pub(crate) value: f64,
    pub(crate) size: f64,
    pub(crate) team: String,
    pub(crate) after: Vec<String>,
}

impl NewCardFields {
    /// Reads `priority`, `value`, `size`, `team` and `after`. Absent fields take their
    /// defaults (0, 1, 1, "", none); present fields must have the right type.
    pub(crate) fn parse(arg: &serde_json::Value) -> Result<Self, StoreError> {
        let priority = match arg.get("priority") {
            None => 0,
            Some(v) => v
                .as_i64()
                .ok_or(StoreError::Invalid("card priority must be an integer"))?,
        };
        let value = positive_field(arg.get("value"), 1.0, VALUE_MSG)?;
        let size = positive_field(arg.get("size"), 1.0, SIZE_MSG)?;
        let team = match arg.get("team") {
            None => String::new(),
            Some(v) => {
                let t = v
                    .as_str()
                    .ok_or(StoreError::Invalid("card team must be a string"))?;
                check_team(t)?;
                t.to_owned()
            }
        };
        let after = match arg.get("after") {
            None => Vec::new(),
            Some(v) => {
                let ids: Result<Vec<String>, StoreError> = v
                    .as_array()
                    .ok_or(StoreError::Invalid(AFTER_MSG))?
                    .iter()
                    .map(|id| {
                        id.as_str()
                            .map(str::to_owned)
                            .ok_or(StoreError::Invalid(AFTER_MSG))
                    })
                    .collect();
                ids?
            }
        };
        Ok(Self {
            priority,
            value,
            size,
            team,
            after,
        })
    }

    /// Inserts the `card_deps` rows of a card that was just created. Every prerequisite
    /// must exist, and a card cannot depend on itself.
    pub(crate) fn insert_deps(&self, tx: &Transaction, card: &str) -> Result<(), StoreError> {
        for after in &self.after {
            if after == card {
                return Err(StoreError::Invalid("a card cannot depend on itself"));
            }
            require_card(tx, after)?;
            tx.execute(
                "INSERT OR IGNORE INTO card_deps(card, after) VALUES (?,?)",
                params![card, after],
            )?;
        }
        Ok(())
    }
}

fn positive_field(
    v: Option<&serde_json::Value>,
    default: f64,
    msg: &'static str,
) -> Result<f64, StoreError> {
    match v {
        None => Ok(default),
        Some(v) => check_positive(v.as_f64().unwrap_or(f64::NAN), msg),
    }
}

pub(super) fn check_positive(x: f64, msg: &'static str) -> Result<f64, StoreError> {
    if x.is_finite() && x > 0.0 {
        Ok(x)
    } else {
        Err(StoreError::Invalid(msg))
    }
}

pub(super) fn check_team(team: &str) -> Result<(), StoreError> {
    if team.chars().count() > 64 {
        return Err(StoreError::Invalid(
            "card team must be at most 64 characters",
        ));
    }
    Ok(())
}

/// Drops repeated keys, keeping the first occurrence of each.
pub(super) fn unique(keys: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(keys.len());
    for key in keys {
        if !out.contains(&key) {
            out.push(key);
        }
    }
    out
}
