//! Protocol versions and the two request eras.
//!
//! Modern (2026-07-28) requests carry `params._meta["io.modelcontextprotocol/protocolVersion"]`
//! and are stateless; everything else is legacy (2025-xx, `initialize` first on stdio, the
//! `MCP-Protocol-Version` header on HTTP).

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::jsonrpc::RpcError;
use crate::{SERVER_NAME, SERVER_VERSION};

/// Versions served as stateless modern requests.
pub const MODERN_VERSIONS: &[&str] = &["2026-07-28"];
/// Versions served as legacy (initialize-first) sessions.
pub const LEGACY_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26"];
/// Advertised in `server/discover` and in -32022 `data.supported`.
pub const SUPPORTED_VERSIONS: &[&str] = &["2026-07-28", "2025-11-25"];
/// The counter-offer for an unknown `initialize` version.
pub const LEGACY_LATEST: &str = "2025-11-25";
/// Legacy HTTP request without an `MCP-Protocol-Version` header.
pub const HTTP_DEFAULT_LEGACY: &str = "2025-03-26";

pub const META_PROTOCOL_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
pub const META_CLIENT_INFO: &str = "io.modelcontextprotocol/clientInfo";
pub const META_CLIENT_CAPS: &str = "io.modelcontextprotocol/clientCapabilities";
pub const META_SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";
/// Unprefixed key inside `_meta`: string or integer.
pub const META_PROGRESS_TOKEN: &str = "progressToken";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Era {
    Modern,
    Legacy,
}

/// `clientInfo` as sent by the peer. Strings read from the wire are truncated to 256 bytes
/// (name) / 64 bytes (version) at a char boundary.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientInfo {
    pub name: String,
    pub version: String,
}

/// What a modern request's `_meta` carries.
#[derive(Debug, Clone, PartialEq)]
pub struct RequestMeta {
    pub era: Era,
    pub version: String,
    pub client_info: Option<ClientInfo>,
    pub client_capabilities: Value,
    pub progress_token: Option<Value>,
}

/// Read the modern `_meta` of a request's params.
///
/// `Ok(None)` = no `params._meta[META_PROTOCOL_VERSION]` key (the request is legacy).
/// With the key: a non-string value -> `Err(-32602 "io.modelcontextprotocol/protocolVersion
/// must be a string")`; a string not in `MODERN_VERSIONS` -> `Err(RpcError::unsupported_version(
/// <version>))` (-32022); missing/non-object `META_CLIENT_CAPS` -> `Err(-32602 "missing
/// io.modelcontextprotocol/clientCapabilities")`; otherwise `Ok(Some(RequestMeta{era: Modern,
/// ..}))` with optional clientInfo and progress token.
pub fn modern_meta(params: &Value) -> Result<Option<RequestMeta>, RpcError> {
    let Some(meta) = params.get("_meta").filter(|m| m.is_object()) else {
        return Ok(None);
    };
    let Some(requested) = meta.get(META_PROTOCOL_VERSION) else {
        return Ok(None);
    };
    let Some(version) = requested.as_str() else {
        return Err(RpcError::invalid_params(
            "io.modelcontextprotocol/protocolVersion must be a string",
        ));
    };
    if !is_modern(version) {
        return Err(RpcError::unsupported_version(version));
    }
    let Some(client_capabilities) = meta.get(META_CLIENT_CAPS).filter(|c| c.is_object()) else {
        return Err(RpcError::invalid_params(
            "missing io.modelcontextprotocol/clientCapabilities",
        ));
    };
    Ok(Some(RequestMeta {
        era: Era::Modern,
        version: version.to_owned(),
        client_info: meta.get(META_CLIENT_INFO).and_then(client_info_from),
        client_capabilities: client_capabilities.clone(),
        progress_token: progress_token(params),
    }))
}

/// `params._meta.progressToken` when it is a string or an integer number (any era).
pub fn progress_token(params: &Value) -> Option<Value> {
    let token = params.get("_meta")?.get(META_PROGRESS_TOKEN)?;
    (token.is_string() || token.is_i64() || token.is_u64()).then(|| token.clone())
}

/// Legacy `initialize` negotiation: a version in `LEGACY_VERSIONS` is answered as is; any
/// other value (including a modern version or garbage) -> `LEGACY_LATEST`.
pub fn negotiate_legacy(requested: &str) -> &'static str {
    LEGACY_VERSIONS
        .iter()
        .find(|v| **v == requested)
        .copied()
        .unwrap_or(LEGACY_LATEST)
}

/// `v` is in `MODERN_VERSIONS`.
pub fn is_modern(v: &str) -> bool {
    MODERN_VERSIONS.contains(&v)
}

/// `v` is in `LEGACY_VERSIONS`.
pub fn is_legacy(v: &str) -> bool {
    LEGACY_VERSIONS.contains(&v)
}

/// `{name, version}` from a JSON object (strings truncated as for `ClientInfo`); `None` when
/// `v` is not an object with a string `name` (a missing `version` becomes "").
pub fn client_info_from(v: &Value) -> Option<ClientInfo> {
    let obj = v.as_object()?;
    let name = obj.get("name")?.as_str()?;
    let version = obj.get("version").and_then(Value::as_str).unwrap_or("");
    Some(ClientInfo {
        name: truncate_at_char_boundary(name, 256),
        version: truncate_at_char_boundary(version, 64),
    })
}

/// `{"name": SERVER_NAME, "version": SERVER_VERSION}`.
pub fn server_info() -> Value {
    json!({"name": SERVER_NAME, "version": SERVER_VERSION})
}

/// Make a result a modern result: add `"resultType":"complete"` and merge
/// `"_meta":{META_SERVER_INFO: server_info()}` into the result object (a non-object result is
/// returned unchanged).
pub fn modernize(result: Value) -> Value {
    match result {
        Value::Object(mut map) => {
            map.entry("resultType").or_insert_with(|| json!("complete"));
            let meta = map.entry("_meta").or_insert_with(|| json!({}));
            if !meta.is_object() {
                *meta = json!({});
            }
            if let Some(meta) = meta.as_object_mut() {
                meta.insert(META_SERVER_INFO.to_owned(), server_info());
            }
            Value::Object(map)
        }
        other => other,
    }
}

/// The longest prefix of `s` that is at most `max_bytes` long and ends on a char boundary.
pub(crate) fn truncate_at_char_boundary(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_owned();
    }
    let end = s
        .char_indices()
        .map(|(i, c)| i + c.len_utf8())
        .take_while(|&e| e <= max_bytes)
        .last()
        .unwrap_or(0);
    s.get(..end).unwrap_or_default().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::jsonrpc::{INVALID_PARAMS, UNSUPPORTED_VERSION};

    const CAPS: &str = "io.modelcontextprotocol/clientCapabilities";

    #[test]
    fn negotiate_legacy_echoes_known_legacy_versions() {
        assert_eq!(negotiate_legacy("2025-06-18"), "2025-06-18");
        assert_eq!(negotiate_legacy("2025-03-26"), "2025-03-26");
        assert_eq!(negotiate_legacy("2025-11-25"), "2025-11-25");
    }

    #[test]
    fn negotiate_legacy_counter_offers_latest_for_anything_else() {
        assert_eq!(negotiate_legacy("2026-07-28"), "2025-11-25");
        assert_eq!(negotiate_legacy("garbage"), "2025-11-25");
        assert_eq!(negotiate_legacy(""), "2025-11-25");
    }

    #[test]
    fn modern_and_legacy_sets_are_disjoint() {
        assert!(is_modern("2026-07-28"));
        assert!(!is_legacy("2026-07-28"));
        assert!(is_legacy("2025-11-25"));
        assert!(!is_modern("2025-11-25"));
        assert!(!is_modern("garbage"));
        assert!(!is_legacy("garbage"));
    }

    #[test]
    fn modern_meta_is_none_without_a_protocol_version_key() {
        assert_eq!(modern_meta(&json!({})), Ok(None));
        assert_eq!(modern_meta(&json!({"_meta": {}})), Ok(None));
        assert_eq!(modern_meta(&json!({"_meta": "x"})), Ok(None));
        assert_eq!(
            modern_meta(&json!({"_meta": {"progressToken": 1}})),
            Ok(None)
        );
    }

    #[test]
    fn modern_meta_is_none_for_non_object_params() {
        assert_eq!(modern_meta(&Value::Null), Ok(None));
        assert_eq!(modern_meta(&json!([1, 2])), Ok(None));
        assert_eq!(modern_meta(&json!("text")), Ok(None));
    }

    #[test]
    fn modern_meta_reads_client_info_and_integer_progress_token() {
        let params = json!({"_meta": {
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            CAPS: {"sampling": {}},
            "io.modelcontextprotocol/clientInfo": {"name": "claude-code", "version": "2.1.0"},
            "progressToken": 7
        }});
        let meta = modern_meta(&params).expect("ok").expect("modern");
        assert_eq!(
            meta,
            RequestMeta {
                era: Era::Modern,
                version: "2026-07-28".into(),
                client_info: Some(ClientInfo {
                    name: "claude-code".into(),
                    version: "2.1.0".into(),
                }),
                client_capabilities: json!({"sampling": {}}),
                progress_token: Some(json!(7)),
            }
        );
    }

    #[test]
    fn modern_meta_accepts_string_progress_token_and_no_client_info() {
        let params = json!({"_meta": {
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            CAPS: {},
            "progressToken": "abc"
        }});
        let meta = modern_meta(&params).expect("ok").expect("modern");
        assert_eq!(meta.client_info, None);
        assert_eq!(meta.progress_token, Some(json!("abc")));
    }

    #[test]
    fn modern_meta_missing_or_non_object_capabilities_is_invalid_params() {
        let missing = json!({"_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28"}});
        let array = json!({"_meta": {
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            CAPS: []
        }});
        for params in [missing, array] {
            let err = modern_meta(&params).expect_err("must reject");
            assert_eq!(err.code, INVALID_PARAMS);
            assert_eq!(
                err.message,
                "missing io.modelcontextprotocol/clientCapabilities"
            );
            assert_eq!(err.data, None);
        }
    }

    #[test]
    fn modern_meta_unknown_version_is_unsupported_with_supported_list() {
        let params = json!({"_meta": {
            "io.modelcontextprotocol/protocolVersion": "2025-11-25",
            CAPS: {}
        }});
        let err = modern_meta(&params).expect_err("legacy version in _meta");
        assert_eq!(err.code, UNSUPPORTED_VERSION);
        assert_eq!(err.message, "Unsupported protocol version");
        assert_eq!(
            err.data,
            Some(json!({"supported": ["2026-07-28", "2025-11-25"], "requested": "2025-11-25"}))
        );
    }

    #[test]
    fn modern_meta_non_string_version_is_invalid_params() {
        for bad in [json!(20260728), json!(null), json!(["2026-07-28"])] {
            let params = json!({"_meta": {
                "io.modelcontextprotocol/protocolVersion": bad,
                CAPS: {}
            }});
            let err = modern_meta(&params).expect_err("must reject");
            assert_eq!(err.code, INVALID_PARAMS);
            assert_eq!(
                err.message,
                "io.modelcontextprotocol/protocolVersion must be a string"
            );
        }
    }

    #[test]
    fn progress_token_accepts_string_and_integer_only() {
        assert_eq!(
            progress_token(&json!({"_meta": {"progressToken": "t"}})),
            Some(json!("t"))
        );
        assert_eq!(
            progress_token(&json!({"_meta": {"progressToken": -3}})),
            Some(json!(-3))
        );
        assert_eq!(
            progress_token(&json!({"_meta": {"progressToken": u64::MAX}})),
            Some(json!(u64::MAX))
        );
        assert_eq!(
            progress_token(&json!({"_meta": {"progressToken": 1.5}})),
            None
        );
        assert_eq!(
            progress_token(&json!({"_meta": {"progressToken": true}})),
            None
        );
        assert_eq!(progress_token(&json!({"_meta": {}})), None);
        assert_eq!(progress_token(&json!({"_meta": "x"})), None);
        assert_eq!(progress_token(&Value::Null), None);
    }

    #[test]
    fn client_info_requires_a_string_name() {
        assert_eq!(client_info_from(&json!({"version": "1"})), None);
        assert_eq!(client_info_from(&json!({"name": 5})), None);
        assert_eq!(client_info_from(&json!("agora")), None);
        assert_eq!(client_info_from(&Value::Null), None);
    }

    #[test]
    fn client_info_defaults_a_missing_or_non_string_version_to_empty() {
        let missing = client_info_from(&json!({"name": "a"}));
        let numeric = client_info_from(&json!({"name": "a", "version": 2}));
        let expected = Some(ClientInfo {
            name: "a".into(),
            version: String::new(),
        });
        assert_eq!(missing, expected);
        assert_eq!(numeric, expected);
    }

    #[test]
    fn client_info_name_is_truncated_at_a_char_boundary() {
        // 255 ASCII bytes, then a 2-byte char: the char would end at byte 257 > 256.
        let name = format!("{}é{}", "a".repeat(255), "b");
        let info = client_info_from(&json!({"name": name, "version": "1"})).expect("name");
        assert_eq!(info.name, "a".repeat(255));
        assert_eq!(info.version, "1");
    }

    #[test]
    fn client_info_name_at_exact_limit_is_kept_whole() {
        let name = "n".repeat(256);
        let info = client_info_from(&json!({"name": name.clone()})).expect("name");
        assert_eq!(info.name, name);
    }

    #[test]
    fn client_info_version_is_truncated_to_64_bytes_at_a_char_boundary() {
        let version = format!("{}€", "v".repeat(63));
        let info = client_info_from(&json!({"name": "a", "version": version})).expect("name");
        assert_eq!(info.version, "v".repeat(63));
    }

    #[test]
    fn server_info_names_the_server_and_its_version() {
        assert_eq!(
            server_info(),
            json!({"name": "agora", "version": SERVER_VERSION})
        );
    }

    #[test]
    fn modernize_adds_result_type_and_keeps_existing_meta_keys() {
        let out = modernize(json!({"tools": [], "_meta": {"other.key": 1}}));
        assert_eq!(out["resultType"], json!("complete"));
        assert_eq!(out["tools"], json!([]));
        assert_eq!(out["_meta"]["other.key"], json!(1));
        assert_eq!(
            out["_meta"]["io.modelcontextprotocol/serverInfo"],
            server_info()
        );
    }

    #[test]
    fn modernize_keeps_an_existing_result_type() {
        let out = modernize(json!({"resultType": "input_required"}));
        assert_eq!(out["resultType"], json!("input_required"));
        assert_eq!(
            out["_meta"]["io.modelcontextprotocol/serverInfo"],
            server_info()
        );
    }

    #[test]
    fn modernize_replaces_a_non_object_meta() {
        let out = modernize(json!({"_meta": 5}));
        assert_eq!(
            out["_meta"],
            json!({"io.modelcontextprotocol/serverInfo": server_info()})
        );
    }

    #[test]
    fn modernize_returns_non_object_results_unchanged() {
        assert_eq!(modernize(json!([1, 2])), json!([1, 2]));
        assert_eq!(modernize(json!("text")), json!("text"));
        assert_eq!(modernize(Value::Null), Value::Null);
    }

    #[test]
    fn truncate_is_identity_within_the_limit_and_stops_before_a_split_char() {
        assert_eq!(truncate_at_char_boundary("abc", 3), "abc");
        assert_eq!(truncate_at_char_boundary("abc", 2), "ab");
        assert_eq!(truncate_at_char_boundary("€", 2), "");
        assert_eq!(truncate_at_char_boundary("", 0), "");
    }
}
