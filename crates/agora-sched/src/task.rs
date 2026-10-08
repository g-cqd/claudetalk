//! Task lists for `order` / `plan`: parsing `--tasks` documents and `--task` specs (ref-ordering sections 2-3).

use serde_json::{Map, Number, Value};

use crate::pyfmt::{float_repr, py_float, py_strip, repr_str};

#[derive(Debug, Clone, PartialEq)]
pub enum Queued {
    Int(i64),
    Float(f64),
    Text(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Task {
    pub index: usize,
    pub id: String,
    pub title: String,
    pub kind: String,
    pub tier: u8,
    pub queued: Option<Queued>,
    pub after: Vec<String>,
    pub size: f64,
    pub value: f64,
    pub files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct InputError(pub String);

pub const VALIDITY_KINDS: [&str; 6] = [
    "validity",
    "correctness",
    "safety",
    "security",
    "data-loss",
    "crash",
];

/// 0 if kind.to_lowercase() (Unicode) is in VALIDITY_KINDS else 1.
pub fn tier_of(kind: &str) -> u8 {
    let lower = kind.to_lowercase();
    if VALIDITY_KINDS.contains(&lower.as_str()) {
        0
    } else {
        1
    }
}

/// Python str() of a JSON value (null->"None", true->"True", ints decimal, floats float_repr, lists/dicts Python repr).
pub fn py_str(v: &serde_json::Value) -> String {
    match v {
        Value::String(text) => text.clone(),
        other => py_repr(other),
    }
}

/// Python repr() of a JSON value. Object keys come out in serde_json's order (sorted, since the `preserve_order`
/// feature is off), not in the document's order.
fn py_repr(v: &Value) -> String {
    match v {
        Value::Null => "None".to_owned(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::Number(number) => number_text(number),
        Value::String(text) => repr_str(text),
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(py_repr).collect();
            format!("[{}]", parts.join(", "))
        }
        Value::Object(map) => {
            let parts: Vec<String> = map
                .iter()
                .map(|(key, value)| format!("{}: {}", repr_str(key), py_repr(value)))
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
    }
}

/// An integer prints in decimal and anything else as a Python float.
fn number_text(number: &Number) -> String {
    if let Some(int) = number.as_i64() {
        int.to_string()
    } else if let Some(int) = number.as_u64() {
        int.to_string()
    } else {
        float_repr(number.as_f64().unwrap_or(f64::NAN))
    }
}

/// --task spec -> ordered (key, value) pairs; split rule `,(?=[A-Za-z_-]+=)` (hand-written, no regex crate).
pub fn parse_spec(spec: &str) -> Result<Vec<(String, String)>, InputError> {
    let mut pairs = Vec::new();
    for part in split_fields(spec) {
        let Some((key, value)) = part.split_once('=') else {
            return Err(InputError(format!(
                "--task {}: {} is not key=value",
                repr_str(spec),
                repr_str(part)
            )));
        };
        let key = py_strip(key);
        if !key.is_empty() {
            pairs.push((key.to_owned(), py_strip(value).to_owned()));
        }
    }
    Ok(pairs)
}

/// Splits at each comma that a `[A-Za-z_-]+=` key follows, as `re.split` with that lookahead does.
fn split_fields(spec: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    for (position, c) in spec.char_indices() {
        if c == ',' && starts_field(spec.get(position + 1..).unwrap_or_default()) {
            parts.push(spec.get(start..position).unwrap_or_default());
            start = position + 1;
        }
    }
    parts.push(spec.get(start..).unwrap_or_default());
    parts
}

/// True when `rest` starts with one or more key characters that are immediately followed by `=`.
fn starts_field(rest: &str) -> bool {
    let name_len = rest.find(|c: char| !is_key_char(c)).unwrap_or(rest.len());
    name_len > 0
        && rest
            .get(name_len..)
            .is_some_and(|tail| tail.starts_with('='))
}

fn is_key_char(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || c == '-'
}

/// Documents (each a list, or an object with a "tasks" list) then specs -> tasks, all checks of ref-ordering sections
/// 2-3 up to and including the mixed-queued check (NOT "no tasks were given", NOT graph checks). Messages exact.
pub fn load_tasks(
    documents: &[serde_json::Value],
    specs: &[String],
) -> Result<Vec<Task>, InputError> {
    let mut raw: Vec<&Map<String, Value>> = Vec::new();
    for document in documents {
        let items = match document {
            Value::Object(map) => map.get("tasks"),
            other => Some(other),
        };
        let Some(Value::Array(items)) = items else {
            return Err(InputError(
                "the JSON must be a list of tasks or an object with a \"tasks\" list".to_owned(),
            ));
        };
        for item in items {
            match item {
                Value::Object(fields) => raw.push(fields),
                _ => {
                    return Err(InputError(
                        "each task in the JSON must be an object".to_owned(),
                    ));
                }
            }
        }
    }
    let mut spec_maps = Vec::with_capacity(specs.len());
    for spec in specs {
        spec_maps.push(spec_fields(spec)?);
    }
    raw.extend(spec_maps.iter());

    let mut tasks = Vec::with_capacity(raw.len());
    for (index, fields) in raw.into_iter().enumerate() {
        tasks.push(make_task(fields, index)?);
    }
    let has_text = tasks
        .iter()
        .any(|task| matches!(task.queued, Some(Queued::Text(_))));
    let has_number = tasks
        .iter()
        .any(|task| matches!(task.queued, Some(Queued::Int(_) | Queued::Float(_))));
    if has_text && has_number {
        return Err(InputError(
            "queued values must be all numbers or all timestamps".to_owned(),
        ));
    }
    Ok(tasks)
}

/// The fields of one --task spec as JSON strings, so specs and documents share one path. A repeated key keeps its
/// last value.
fn spec_fields(spec: &str) -> Result<Map<String, Value>, InputError> {
    let mut fields = Map::new();
    for (key, value) in parse_spec(spec)? {
        fields.insert(key, Value::String(value));
    }
    Ok(fields)
}

/// A task from its fields, checked in Python's order (id, queued type, after, size, value, files). `index` is the
/// position in the input (documents, then specs), which is the default arrival order.
fn make_task(fields: &Map<String, Value>, index: usize) -> Result<Task, InputError> {
    let raw_id = fields.get("id").map_or_else(String::new, py_str);
    let id = py_strip(&raw_id).to_owned();
    if id.is_empty() {
        return Err(InputError(format!(
            "task at position {} has no id",
            index + 1
        )));
    }
    let queued = queued_of(fields.get("queued"), &id)?;
    let after = list_of(fields.get("after"))?;
    let size = positive(fields.get("size"), "size", &id)?;
    let value = positive(fields.get("value"), "value", &id)?;
    let files = list_of(fields.get("files"))?;
    let title = fields.get("title").map_or_else(String::new, py_str);
    let kind = fields
        .get("kind")
        .map_or_else(|| "other".to_owned(), py_str);
    Ok(Task {
        index,
        id,
        title,
        tier: tier_of(&kind),
        kind,
        queued,
        after,
        size,
        value,
        files,
    })
}

/// `queued`: absent or null is None, a number stays a number, a string becomes a float when Python float() reads it
/// and text otherwise. A bool, list or object is an error.
fn queued_of(raw: Option<&Value>, id: &str) -> Result<Option<Queued>, InputError> {
    match raw {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => Ok(Some(match number.as_i64() {
            Some(int) => Queued::Int(int),
            // A u64 above i64::MAX lands here as an approximate float (Python would keep the exact int).
            None => Queued::Float(number.as_f64().unwrap_or(f64::NAN)),
        })),
        Some(Value::String(text)) => Ok(Some(text_queued(text))),
        Some(_) => Err(InputError(format!(
            "task {id}: queued must be a number or an ISO 8601 time"
        ))),
    }
}

fn text_queued(text: &str) -> Queued {
    py_float(text).map_or_else(|| Queued::Text(text.to_owned()), Queued::Float)
}

/// `_list`: absent or null is empty; a string splits on `+` with empty pieces dropped (pieces not stripped); a JSON
/// list keeps every item as Python str() of it; anything else is an error.
fn list_of(raw: Option<&Value>) -> Result<Vec<String>, InputError> {
    match raw {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(text)) => Ok(text
            .split('+')
            .filter(|piece| !piece.is_empty())
            .map(str::to_owned)
            .collect()),
        Some(Value::Array(items)) => Ok(items.iter().map(py_str).collect()),
        Some(other) => Err(InputError(format!(
            "expected a list, not {}",
            py_repr(other)
        ))),
    }
}

/// `_positive`: absent is 1.0; otherwise Python float(raw), which must give a number greater than zero (NaN fails).
fn positive(raw: Option<&Value>, name: &str, id: &str) -> Result<f64, InputError> {
    let Some(raw) = raw else {
        return Ok(1.0);
    };
    let number = float_of(raw).ok_or_else(|| {
        InputError(format!(
            "task {id}: {name} must be a number, not {}",
            py_repr(raw)
        ))
    })?;
    if number.is_nan() || number <= 0.0 {
        return Err(InputError(format!(
            "task {id}: {name} must be greater than zero"
        )));
    }
    Ok(number)
}

/// Python float(raw) for the JSON values that reach `_positive`; None where Python raises TypeError or ValueError.
fn float_of(raw: &Value) -> Option<f64> {
    match raw {
        Value::Bool(flag) => Some(if *flag { 1.0 } else { 0.0 }),
        Value::Number(number) => number.as_f64(),
        Value::String(text) => py_float(text),
        Value::Null | Value::Array(_) | Value::Object(_) => None,
    }
}

#[cfg(test)]
mod tests;
