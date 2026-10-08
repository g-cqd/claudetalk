//! The checks `work.add` applies to a timer's task template. They run when the timer is created, so a template that
//! cannot become a task is refused up front.

use super::exists;
use crate::StoreError;
use crate::identity::need;
use crate::work_ops::checks::path_depth;
use agora_core::Role;
use agora_sched::claims::{
    canonical_path, check_command, check_text, validate_kind, validate_name,
};
use agora_sched::model_ops::AddArgs;
use agora_sched::pyfmt::printable;
use rusqlite::{Connection, OptionalExtension};

/// Longest title a task may carry, in characters.
const TITLE_LIMIT: usize = 200;
/// Longest working directory a task may carry, in characters.
const CWD_LIMIT: usize = 4096;
/// Most files, and most prerequisites, a task may carry (work.py MAX_ITEMS).
const MAX_ITEMS: usize = 1000;
/// The lightest weight a task may carry (S-7); the same floor `work.add` applies.
const MIN_WEIGHT: f64 = 0.1;

/// Checks a template for the timer owned by `owner` (hex) acting with `role`. The owner must own the card the template
/// names, or be Supervisor+. Returns the template with its files canonicalised.
pub(super) fn check_template(
    conn: &Connection,
    owner: &str,
    role: Role,
    template: &AddArgs,
) -> Result<AddArgs, StoreError> {
    if template.id.is_some() {
        return Err(StoreError::Invalid(
            "a timer template takes no task id: its tasks are named <timer>-<n>",
        ));
    }
    if template.files.len() > MAX_ITEMS || template.after.len() > MAX_ITEMS {
        return Err(StoreError::Invalid("too many files or prerequisites"));
    }
    validate_kind(&template.kind).map_err(StoreError::Refused)?;
    check_text("title", &template.title, TITLE_LIMIT).map_err(StoreError::Refused)?;
    if let Some(scope) = &template.scope {
        validate_name("scope", scope).map_err(StoreError::Refused)?;
        if !exists(
            conn,
            "SELECT EXISTS(SELECT 1 FROM work_scopes WHERE id=?)",
            scope,
        )? {
            return Err(StoreError::Refused(format!("unknown scope {scope}")));
        }
    }
    for prerequisite in &template.after {
        validate_name("after", prerequisite).map_err(StoreError::Refused)?;
        if !exists(
            conn,
            "SELECT EXISTS(SELECT 1 FROM work_tasks WHERE id=?)",
            prerequisite,
        )? {
            return Err(StoreError::Refused(format!(
                "after {prerequisite} is not a task"
            )));
        }
    }
    positive(template.size, "size must be a positive number")?;
    positive(template.value, "value must be a positive number")?;
    if !(template.weight.is_finite() && template.weight >= MIN_WEIGHT) {
        return Err(StoreError::Invalid("weight must be at least 0.1"));
    }
    if let Some(argv) = &template.command {
        check_command(argv).map_err(StoreError::Refused)?;
    }
    check_cwd(template)?;
    let files = template
        .files
        .iter()
        .map(|file| template_file(file, template.cwd.as_deref()))
        .collect::<Result<Vec<_>, _>>()?;
    // S-8: the creator's role decides; every fire claims these files under the owner's name.
    let too_broad = if role < Role::Supervisor {
        files.iter().find(|file| path_depth(file) < 2)
    } else {
        None
    };
    if let Some(file) = too_broad {
        return Err(StoreError::Refused(format!(
            "{} is too broad to claim",
            printable(file)
        )));
    }
    if let Some(card) = &template.card {
        validate_name("card", card).map_err(StoreError::Refused)?;
        let owned_by =
            card_owner(conn, card)?.ok_or_else(|| StoreError::NotFound(format!("card {card}")))?;
        if owned_by.as_deref() != Some(owner) {
            need(role, Role::Supervisor)?;
        }
    }
    Ok(AddArgs {
        files,
        ..template.clone()
    })
}

/// A command needs a working directory, and a working directory must be an absolute path. The timer does not check that
/// the directory exists: it may be created or removed between creation and a fire.
fn check_cwd(template: &AddArgs) -> Result<(), StoreError> {
    match &template.cwd {
        Some(cwd) => {
            check_text("cwd", cwd, CWD_LIMIT).map_err(StoreError::Refused)?;
            if !cwd.starts_with('/') {
                return Err(StoreError::Invalid(
                    "the working directory must be an absolute path",
                ));
            }
            Ok(())
        }
        None if template.command.is_some() => {
            Err(StoreError::Invalid("a command needs a working directory"))
        }
        None => Ok(()),
    }
}

/// A template file as a canonical absolute path. A relative file needs the template's cwd; `~` is not expanded.
fn template_file(file: &str, cwd: Option<&str>) -> Result<String, StoreError> {
    if file == "~" || file.starts_with("~/") {
        return Err(StoreError::Refused(format!(
            "file {file} is not expanded in a timer template: use an absolute path"
        )));
    }
    if !file.starts_with('/') && cwd.is_none() {
        return Err(StoreError::Refused(format!(
            "file {file} is relative and the template has no cwd"
        )));
    }
    canonical_path(file, cwd.unwrap_or("/"), None).map_err(StoreError::Refused)
}

fn positive(value: f64, message: &'static str) -> Result<(), StoreError> {
    if value.is_finite() && value > 0.0 {
        Ok(())
    } else {
        Err(StoreError::Invalid(message))
    }
}

/// The owner of a card: `None` if the card does not exist, `Some(None)` if it is unclaimed.
pub(super) fn card_owner(
    conn: &Connection,
    card: &str,
) -> Result<Option<Option<String>>, StoreError> {
    Ok(conn
        .query_row("SELECT owner FROM cards WHERE id=?", [card], |r| r.get(0))
        .optional()?)
}
