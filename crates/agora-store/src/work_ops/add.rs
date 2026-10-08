//! Validation and insertion of a new task: `work.add` (queued) and the inline `work.run` (a job claimed and accepted
//! by its author as it is created). Rules: ref-queue-cli sections 1, 2 and 4, D5 and D7.

use super::checks::{
    MAX_ITEMS, TEXT_MAX, TITLE_MAX, check_card, check_text_field, detail_text, event, path_depth,
    refuse, scope_exists, task_exists,
};
use super::queue::check_files;
use super::request::signatures_for;
use crate::StoreError;
use agora_core::Role;
use agora_sched::claims;
use agora_sched::model_ops::AddArgs;
use agora_sched::signature::Signatures;
use agora_sched::textfmt::number;
use rusqlite::{Transaction, params};
use serde_json::json;

/// How the task enters the queue.
#[derive(Debug, Clone, Copy)]
pub(super) enum Placement {
    /// `work.add`: queued, waiting for a claim.
    Queued,
    /// `work.run` with a command: claimed and accepted by the author; `cpus` bounds the weight.
    Inline { cpus: f64 },
}

/// The lightest weight a task may carry (S-7).
const MIN_WEIGHT: f64 = 0.1;

fn finite_positive(value: f64) -> bool {
    value.is_finite() && value > 0.0
}

/// Validates `add` and inserts the task as `actor`. Returns its id. The id is `add.id` when given, else `task-<n>`
/// (queued) or `job-<n>` (inline), with n one past the highest seq.
pub(super) fn insert_task(
    tx: &Transaction,
    now: i64,
    actor: &str,
    role: Role,
    add: &AddArgs,
    place: Placement,
) -> Result<String, StoreError> {
    let inline = matches!(place, Placement::Inline { .. });
    refuse(claims::validate_kind(&add.kind))?;
    check_text_field("title", &add.title, TITLE_MAX)?;
    let (size, value) = if inline {
        (1.0, 1.0)
    } else {
        (add.size, add.value)
    };
    if !(add.weight.is_finite() && add.weight >= MIN_WEIGHT) {
        return Err(StoreError::Invalid("weight must be at least 0.1"));
    }
    if let Placement::Inline { cpus } = place {
        let limit = cpus.max(1.0);
        if add.weight > limit {
            return Err(StoreError::Refused(format!(
                "--weight {} is more than the {} effective CPUs of this machine",
                number(add.weight),
                number(limit)
            )));
        }
    }
    if !finite_positive(size) {
        return Err(StoreError::Invalid("size must be a number above zero"));
    }
    if !finite_positive(value) {
        return Err(StoreError::Invalid("value must be a number above zero"));
    }
    match (&add.command, inline) {
        (None, true) => return Err(StoreError::Invalid("a new job needs a command to run")),
        (Some(argv), _) => refuse(claims::check_command(argv))?,
        (None, false) => {}
    }
    // O-43: the working directory is stored and digested in its canonical form, so a symlink never changes what a
    // digest names; the supervisor compares the real path against this same form (S-3).
    let cwd = match add.cwd.as_deref().filter(|dir| !dir.is_empty()) {
        Some(dir) => {
            check_text_field("working directory", dir, TEXT_MAX)?;
            if !dir.starts_with('/') {
                return Err(StoreError::Invalid(
                    "the working directory must be an absolute path",
                ));
            }
            let real = std::fs::canonicalize(dir)
                .ok()
                .filter(|real| real.is_dir())
                .ok_or_else(|| StoreError::Refused(format!("{dir} is not a directory")))?;
            let real = real
                .to_str()
                .ok_or_else(|| StoreError::Refused(format!("{dir} is not a UTF-8 path")))?
                .to_string();
            check_text_field("working directory", &real, TEXT_MAX)?;
            Some(real)
        }
        None => None,
    };
    if add.command.is_some() && cwd.is_none() {
        return Err(StoreError::Invalid("a command needs a working directory"));
    }
    if inline && !add.after.is_empty() {
        return Err(StoreError::Invalid("a new job takes no prerequisites"));
    }
    if add.files.len() > MAX_ITEMS {
        return Err(StoreError::Invalid("more than 1000 files"));
    }
    let mut files: Vec<String> = Vec::with_capacity(add.files.len());
    for raw in &add.files {
        if cwd.is_none() && !raw.starts_with('/') {
            return Err(StoreError::Invalid(
                "a relative file path needs a working directory",
            ));
        }
        let path = refuse(claims::canonical_path(
            raw,
            cwd.as_deref().unwrap_or("/"),
            None,
        ))?;
        check_text_field("file path", &path, TEXT_MAX)?;
        if role < Role::Supervisor && path_depth(&path) < 2 {
            return Err(StoreError::Refused(format!("{path} is too broad to claim")));
        }
        if !files.contains(&path) {
            files.push(path);
        }
    }
    let mut after: Vec<String> = Vec::with_capacity(add.after.len());
    for item in &add.after {
        if !after.contains(item) {
            after.push(item.clone());
        }
    }
    if after.len() > MAX_ITEMS {
        return Err(StoreError::Invalid("more than 1000 prerequisites"));
    }
    for item in &after {
        refuse(claims::validate_name("--after", item))?;
    }
    let scope = add.scope.clone().filter(|s| !s.is_empty());
    if let Some(name) = &scope {
        refuse(claims::validate_name("scope", name))?;
        scope_exists(tx, name)?;
    }
    if let Some(card) = add.card.as_deref().filter(|c| !c.is_empty()) {
        check_card(tx, card, actor, role)?;
    }
    let what = if inline { "job" } else { "task" };
    let id = match add.id.as_deref() {
        Some(id) => {
            refuse(claims::validate_name("task", id))?;
            id.to_string()
        }
        None => new_id(tx, what)?,
    };
    if task_exists(tx, &id)? {
        return Err(StoreError::Refused(format!("task {id} exists already")));
    }
    for item in &after {
        if !task_exists(tx, item)? {
            return Err(StoreError::Refused(format!(
                "task {id} is after unknown task {item}"
            )));
        }
    }
    let (state, agent, claimed) = if inline {
        check_files(tx, now, &format!("job {id}"), &files, actor)?;
        ("claimed", Some(actor), Some(now))
    } else {
        ("queued", None, None)
    };
    let sigs: Option<Signatures> = add
        .command
        .as_ref()
        .map(|argv| signatures_for(argv, cwd.as_deref()));
    let command_json = match &add.command {
        Some(argv) => Some(
            serde_json::to_string(argv)
                .map_err(|_| StoreError::Invalid("command could not be stored"))?,
        ),
        None => None,
    };
    let after_json = serde_json::to_string(&after)
        .map_err(|_| StoreError::Invalid("prerequisites could not be stored"))?;
    let files_json = serde_json::to_string(&files)
        .map_err(|_| StoreError::Invalid("files could not be stored"))?;
    let sig_full = sigs.as_ref().map(|s| s.full.clone());
    let sig_head = sigs.as_ref().map(|s| s.head.clone());
    let sig_prog = sigs.as_ref().map(|s| s.prog.clone());
    let sig_tool = sigs.as_ref().map(|s| s.tool.clone());
    tx.execute(
        "INSERT INTO work_tasks(id, title, kind, scope, author, queued, after, size, value, files, command, cwd, \
         weight, state, agent, accepted, claimed, card, sig_full, sig_head, sig_prog, sig_tool) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22)",
        params![
            id,
            add.title,
            add.kind,
            scope,
            actor,
            now,
            after_json,
            size,
            value,
            files_json,
            command_json,
            cwd,
            add.weight,
            state,
            agent,
            agent,
            claimed,
            add.card.as_deref().filter(|c| !c.is_empty()),
            sig_full,
            sig_head,
            sig_prog,
            sig_tool,
        ],
    )?;
    event(
        tx,
        now,
        actor,
        "work.added",
        &id,
        json!({"kind": add.kind, "scope": scope, "detail": detail_text(&add.title, &id)}),
    )?;
    if inline {
        event(
            tx,
            now,
            actor,
            "work.claimed",
            &id,
            json!({"scope": scope, "detail": "typed inline with work run"}),
        )?;
    }
    Ok(id)
}

/// `<prefix>-<n>` with n one past the highest seq, skipping ids that already exist.
fn new_id(tx: &Transaction, prefix: &str) -> Result<String, StoreError> {
    let mut n: i64 = tx.query_row("SELECT COALESCE(MAX(seq), 0) FROM work_tasks", [], |r| {
        r.get(0)
    })?;
    loop {
        n = n.saturating_add(1);
        let candidate = format!("{prefix}-{n}");
        if !task_exists(tx, &candidate)? {
            return Ok(candidate);
        }
    }
}
