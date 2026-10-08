//! JSON-RPC 2.0 wire layer: parse an incoming message into an `Incoming`, build responses.
//!
//! Spec rules (SPEC Part 3, "Protocol facts"): invalid JSON -> -32700 "Parse error" (id
//! null); a JSON array (batch) -> -32600 "batching is not supported" (id null); `jsonrpc !=
//! "2.0"` or a non-string method -> -32600 (echo the id when it is a string or an integer
//! number); an id that is neither a string nor an integer number -> Invalid with id Null.
//! `params: null` is the same as absent. Any other non-object `params` -> -32602 for a request
//! (echoing its id); a notification has nothing to answer, so it is dropped as `Response`.
//! A message with `result` or `error` and no `method` is a client response (`Response`,
//! ignored by the engine). A message with a method and no `id` is a notification.

use serde_json::{Value, json};

use crate::version::SUPPORTED_VERSIONS;

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;
/// 2026-07-28: a transport header disagrees with the body.
pub const HEADER_MISMATCH: i64 = -32020;
/// 2026-07-28: unsupported protocol version (`data` lists the supported ones).
pub const UNSUPPORTED_VERSION: i64 = -32022;

/// Largest accepted message: 8 MiB (stdio line, HTTP body).
pub const MAX_MESSAGE_BYTES: usize = 8 * 1024 * 1024;

/// A JSON-RPC error object.
#[derive(Debug, Clone, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl RpcError {
    /// An error with `code` and `message` and no data.
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    /// The same error with `data` attached.
    pub fn with_data(self, data: Value) -> Self {
        Self {
            data: Some(data),
            ..self
        }
    }

    /// -32700 "Parse error".
    pub fn parse_error() -> Self {
        Self::new(PARSE_ERROR, "Parse error")
    }

    /// -32600 with `msg`.
    pub fn invalid_request(msg: impl Into<String>) -> Self {
        Self::new(INVALID_REQUEST, msg)
    }

    /// -32601 "Method not found: <method>".
    pub fn method_not_found(method: &str) -> Self {
        Self::new(METHOD_NOT_FOUND, format!("Method not found: {method}"))
    }

    /// -32602 with `msg`.
    pub fn invalid_params(msg: impl Into<String>) -> Self {
        Self::new(INVALID_PARAMS, msg)
    }

    /// -32603 "Internal error" (no detail: details go to the log, never to the peer).
    pub fn internal() -> Self {
        Self::new(INTERNAL_ERROR, "Internal error")
    }

    /// -32022 "Unsupported protocol version" with data
    /// `{"supported": SUPPORTED_VERSIONS, "requested": requested}`.
    pub fn unsupported_version(requested: &str) -> Self {
        Self::new(UNSUPPORTED_VERSION, "Unsupported protocol version").with_data(json!({
            "supported": SUPPORTED_VERSIONS,
            "requested": requested,
        }))
    }

    /// -32020 "Header mismatch: <header>".
    pub fn header_mismatch(header: &str) -> Self {
        Self::new(HEADER_MISMATCH, format!("Header mismatch: {header}"))
    }
}

/// One classified incoming message.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    /// A request: `id` is a string or an integer number; `params` is `Value::Null` when absent.
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    /// A notification (no id); `params` is `Value::Null` when absent.
    Notification { method: String, params: Value },
    /// A client response (result/error without method): ignored by the engine. Also a
    /// notification (no id) whose `params` is present but not an object: it has no id to
    /// answer, so it is dropped the same way instead of being reported.
    Response,
    /// Malformed: answer with `error`, echoing `id` (Null when unknown).
    Invalid { error: RpcError, id: Value },
}

/// Parse raw bytes (one message, no framing) and classify them. Never panics on any input.
pub fn parse(bytes: &[u8]) -> Incoming {
    match serde_json::from_slice::<Value>(bytes) {
        Err(_) => invalid(RpcError::parse_error(), Value::Null),
        Ok(Value::Array(_)) => invalid(
            RpcError::invalid_request("batching is not supported"),
            Value::Null,
        ),
        Ok(v) => classify(v),
    }
}

/// Classify an already parsed JSON value (rules in the module doc).
pub fn classify(v: Value) -> Incoming {
    let Value::Object(mut obj) = v else {
        return invalid(
            RpcError::invalid_request("not a JSON-RPC object"),
            Value::Null,
        );
    };
    // The id to echo on errors about the message as a whole: only a valid id is echoed.
    let echo = match obj.get("id") {
        Some(id) if is_valid_id(id) => id.clone(),
        _ => Value::Null,
    };
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return invalid(RpcError::invalid_request("jsonrpc must be \"2.0\""), echo);
    }
    let method = match obj.remove("method") {
        Some(Value::String(method)) => method,
        Some(_) => {
            return invalid(RpcError::invalid_request("method must be a string"), echo);
        }
        None if obj.contains_key("result") || obj.contains_key("error") => {
            return Incoming::Response;
        }
        None => return invalid(RpcError::invalid_request("missing method"), echo),
    };
    let id = match obj.remove("id") {
        None => None,
        Some(id) if is_valid_id(&id) => Some(id),
        Some(_) => return invalid(RpcError::invalid_request("invalid id"), Value::Null),
    };
    let params = match obj.remove("params") {
        None | Some(Value::Null) => Value::Null,
        Some(params @ Value::Object(_)) => params,
        Some(_) => {
            return match id {
                Some(id) => invalid(RpcError::invalid_params("params must be an object"), id),
                None => Incoming::Response,
            };
        }
    };
    match id {
        Some(id) => Incoming::Request { id, method, params },
        None => Incoming::Notification { method, params },
    }
}

/// A request id must be a string or an integer number (i64 or u64); floats, null, bool,
/// object and array are rejected.
fn is_valid_id(id: &Value) -> bool {
    id.is_string() || id.is_i64() || id.is_u64()
}

fn invalid(error: RpcError, id: Value) -> Incoming {
    Incoming::Invalid { error, id }
}

/// `{"jsonrpc":"2.0","id":id,"result":result}`.
pub fn response(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

/// `{"jsonrpc":"2.0","id":id,"error":{"code","message"[,"data"]}}`.
pub fn error_response(id: &Value, e: &RpcError) -> Value {
    let mut error = serde_json::Map::new();
    error.insert("code".to_owned(), Value::from(e.code));
    error.insert("message".to_owned(), Value::from(e.message.as_str()));
    if let Some(data) = &e.data {
        error.insert("data".to_owned(), data.clone());
    }
    json!({"jsonrpc": "2.0", "id": id, "error": error})
}

/// `{"jsonrpc":"2.0","method":method,"params":params}` (no id).
pub fn notification(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": params})
}

/// The `error.code` of a response object, if it is an error response.
pub fn error_code(response: &Value) -> Option<i64> {
    response.get("error")?.get("code")?.as_i64()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invalid_request(msg: &str, id: Value) -> Incoming {
        Incoming::Invalid {
            error: RpcError::invalid_request(msg),
            id,
        }
    }

    #[test]
    fn parse_error_on_invalid_json() {
        assert_eq!(
            parse(b"{not json"),
            Incoming::Invalid {
                error: RpcError::parse_error(),
                id: Value::Null,
            }
        );
        assert_eq!(
            parse(b""),
            Incoming::Invalid {
                error: RpcError::parse_error(),
                id: Value::Null,
            }
        );
    }

    #[test]
    fn batch_array_is_rejected() {
        let batch = br#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#;
        assert_eq!(
            parse(batch),
            invalid_request("batching is not supported", Value::Null)
        );
    }

    #[test]
    fn parse_classifies_a_valid_request() {
        assert_eq!(
            parse(br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#),
            Incoming::Request {
                id: json!(1),
                method: "ping".to_owned(),
                params: Value::Null,
            }
        );
    }

    #[test]
    fn non_object_is_rejected() {
        for v in [json!(5), json!("x"), json!(null), json!(true)] {
            assert_eq!(
                classify(v),
                invalid_request("not a JSON-RPC object", Value::Null)
            );
        }
    }

    #[test]
    fn wrong_jsonrpc_version_is_rejected_and_echoes_valid_id() {
        assert_eq!(
            classify(json!({"jsonrpc": "1.0", "id": 7, "method": "ping"})),
            invalid_request("jsonrpc must be \"2.0\"", json!(7))
        );
        assert_eq!(
            classify(json!({"id": "a", "method": "ping"})),
            invalid_request("jsonrpc must be \"2.0\"", json!("a"))
        );
    }

    #[test]
    fn wrong_jsonrpc_with_invalid_id_echoes_null() {
        assert_eq!(
            classify(json!({"jsonrpc": "1.0", "id": 1.5, "method": "ping"})),
            invalid_request("jsonrpc must be \"2.0\"", Value::Null)
        );
    }

    #[test]
    fn result_or_error_without_method_is_a_response() {
        assert_eq!(
            classify(json!({"jsonrpc": "2.0", "id": 1, "result": {}})),
            Incoming::Response
        );
        assert_eq!(
            classify(json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -1, "message": "x"}})),
            Incoming::Response
        );
    }

    #[test]
    fn method_present_but_not_a_string_is_rejected() {
        assert_eq!(
            classify(json!({"jsonrpc": "2.0", "id": 3, "method": null})),
            invalid_request("method must be a string", json!(3))
        );
        // A method key alongside result is not a response: the method is checked first.
        assert_eq!(
            classify(json!({"jsonrpc": "2.0", "id": 3, "method": 5, "result": {}})),
            invalid_request("method must be a string", json!(3))
        );
    }

    #[test]
    fn missing_method_without_result_or_error_is_rejected() {
        assert_eq!(
            classify(json!({"jsonrpc": "2.0", "id": 1})),
            invalid_request("missing method", json!(1))
        );
    }

    #[test]
    fn message_without_id_is_a_notification() {
        assert_eq!(
            classify(json!({"jsonrpc": "2.0", "method": "notifications/initialized"})),
            Incoming::Notification {
                method: "notifications/initialized".to_owned(),
                params: Value::Null,
            }
        );
        assert_eq!(
            classify(json!({"jsonrpc": "2.0", "method": "n", "params": {"a": 1}})),
            Incoming::Notification {
                method: "n".to_owned(),
                params: json!({"a": 1}),
            }
        );
    }

    #[test]
    fn string_id_is_a_request() {
        assert_eq!(
            classify(json!({"jsonrpc": "2.0", "id": "abc", "method": "ping", "params": {"a": 1}})),
            Incoming::Request {
                id: json!("abc"),
                method: "ping".to_owned(),
                params: json!({"a": 1}),
            }
        );
    }

    #[test]
    fn integer_id_is_a_request() {
        for id in [json!(42), json!(-1), json!(u64::MAX)] {
            assert_eq!(
                classify(json!({"jsonrpc": "2.0", "id": id.clone(), "method": "ping"})),
                Incoming::Request {
                    id,
                    method: "ping".to_owned(),
                    params: Value::Null,
                }
            );
        }
    }

    #[test]
    fn float_id_is_rejected() {
        assert_eq!(
            classify(json!({"jsonrpc": "2.0", "id": 1.5, "method": "ping"})),
            invalid_request("invalid id", Value::Null)
        );
        assert_eq!(
            classify(json!({"jsonrpc": "2.0", "id": 1.0, "method": "ping"})),
            invalid_request("invalid id", Value::Null)
        );
    }

    #[test]
    fn null_id_is_rejected() {
        assert_eq!(
            classify(json!({"jsonrpc": "2.0", "id": null, "method": "ping"})),
            invalid_request("invalid id", Value::Null)
        );
    }

    #[test]
    fn bool_object_and_array_ids_are_rejected() {
        for id in [json!(true), json!({}), json!([1])] {
            assert_eq!(
                classify(json!({"jsonrpc": "2.0", "id": id, "method": "ping"})),
                invalid_request("invalid id", Value::Null)
            );
        }
    }

    fn invalid_params(msg: &str, id: Value) -> Incoming {
        Incoming::Invalid {
            error: RpcError::invalid_params(msg),
            id,
        }
    }

    #[test]
    fn non_object_params_are_rejected_with_invalid_params_and_echo_the_id() {
        assert_eq!(
            classify(json!({"jsonrpc": "2.0", "id": 1, "method": "ping", "params": [1, 2]})),
            invalid_params("params must be an object", json!(1))
        );
        assert_eq!(
            classify(json!({"jsonrpc": "2.0", "id": "s", "method": "ping", "params": "x"})),
            invalid_params("params must be an object", json!("s"))
        );
        assert_eq!(
            classify(json!({"jsonrpc": "2.0", "id": 2, "method": "ping", "params": 0})),
            invalid_params("params must be an object", json!(2))
        );
    }

    #[test]
    fn non_object_params_on_a_notification_are_ignored_as_a_response() {
        assert_eq!(
            classify(json!({"jsonrpc": "2.0", "method": "n", "params": 3})),
            Incoming::Response
        );
        assert_eq!(
            classify(json!({"jsonrpc": "2.0", "method": "n", "params": [1]})),
            Incoming::Response
        );
    }

    #[test]
    fn null_params_are_absent() {
        assert_eq!(
            classify(json!({"jsonrpc": "2.0", "id": 1, "method": "ping", "params": null})),
            Incoming::Request {
                id: json!(1),
                method: "ping".to_owned(),
                params: Value::Null,
            }
        );
        assert_eq!(
            classify(json!({"jsonrpc": "2.0", "method": "n", "params": null})),
            Incoming::Notification {
                method: "n".to_owned(),
                params: Value::Null,
            }
        );
    }

    #[test]
    fn constructors_have_the_spec_codes_and_messages() {
        let cases = [
            (RpcError::parse_error(), -32700, "Parse error".to_owned()),
            (RpcError::invalid_request("bad"), -32600, "bad".to_owned()),
            (
                RpcError::method_not_found("resources/list"),
                -32601,
                "Method not found: resources/list".to_owned(),
            ),
            (RpcError::invalid_params("bad"), -32602, "bad".to_owned()),
            (RpcError::internal(), -32603, "Internal error".to_owned()),
            (
                RpcError::header_mismatch("Mcp-Method"),
                -32020,
                "Header mismatch: Mcp-Method".to_owned(),
            ),
            (
                RpcError::unsupported_version("1999-01-01"),
                -32022,
                "Unsupported protocol version".to_owned(),
            ),
        ];
        for (err, code, message) in cases {
            assert_eq!(err.code, code);
            assert_eq!(err.message, message);
        }
        assert_eq!(RpcError::new(-1, "m").data, None);
    }

    #[test]
    fn unsupported_version_data_shape() {
        let err = RpcError::unsupported_version("1999-01-01");
        assert_eq!(
            err.data,
            Some(json!({
                "supported": ["2026-07-28", "2025-11-25"],
                "requested": "1999-01-01",
            }))
        );
    }

    #[test]
    fn response_has_jsonrpc_id_and_result() {
        assert_eq!(
            response(&json!(1), json!({"x": 1})),
            json!({"jsonrpc": "2.0", "id": 1, "result": {"x": 1}})
        );
    }

    #[test]
    fn error_response_without_data_has_no_data_key() {
        let v = error_response(&json!("a"), &RpcError::parse_error());
        assert_eq!(
            v,
            json!({"jsonrpc": "2.0", "id": "a", "error": {"code": -32700, "message": "Parse error"}})
        );
        assert!(v["error"].get("data").is_none());
    }

    #[test]
    fn error_response_with_data_carries_it() {
        let e = RpcError::unsupported_version("x");
        let v = error_response(&Value::Null, &e);
        assert_eq!(v["error"]["code"], json!(-32022));
        assert_eq!(v["error"]["data"], e.data.clone().unwrap_or(Value::Null));
    }

    #[test]
    fn notification_has_no_id() {
        assert_eq!(
            notification("notifications/progress", json!({"progress": 1})),
            json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": {"progress": 1}})
        );
        assert!(notification("n", Value::Null).get("id").is_none());
    }

    #[test]
    fn error_code_reads_error_responses_only() {
        let err = error_response(&json!(1), &RpcError::method_not_found("x"));
        assert_eq!(error_code(&err), Some(-32601));
        assert_eq!(error_code(&response(&json!(1), json!({}))), None);
        assert_eq!(error_code(&json!({"error": {"message": "no code"}})), None);
        assert_eq!(error_code(&json!({})), None);
    }
}
