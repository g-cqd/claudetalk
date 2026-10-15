use super::*;
use std::collections::HashSet;

/// The SPEC tool table in order: name, read_only, destructive, idempotent.
const SPEC: &[(&str, bool, bool, bool)] = &[
    ("whoami", true, false, true),
    ("discover", true, false, true),
    ("chat", false, false, false),
    ("attach", false, false, false),
    ("read", true, false, true),
    ("react", false, false, true),
    ("fetch_file", false, false, false),
    ("inbox", true, false, true),
    ("inbox_wait", true, false, false),
    ("ack", false, false, true),
    ("channel_create", false, false, false),
    ("channel_add", false, false, false),
    ("channel_remove", false, true, false),
    ("channel_rotate", false, false, false),
    ("channel_promote", false, false, true),
    ("channel_demote", false, true, true),
    ("role_grant", false, true, true),
    ("revoke", false, true, true),
    ("status_set", false, false, true),
    ("status_clear", false, false, true),
    ("status_list", true, false, true),
    ("card_create", false, false, false),
    ("card_move", false, false, false),
    ("card_evidence", false, false, false),
    ("card_approve", false, false, false),
    ("card_reclaim", false, true, false),
    ("card_list", true, false, true),
    ("sprint_create", false, false, false),
    ("sprint_add", false, false, false),
    ("sprint_start", false, false, false),
    ("sprint_close", false, false, false),
    ("sprint_summary", true, false, true),
    ("board_summary", true, false, true),
    ("note_publish", false, false, false),
    ("note_search", true, false, true),
    ("delivery_status", true, false, true),
    ("work_scope", false, false, true),
    ("work_add", false, false, false),
    ("work_show", true, false, true),
    ("work_next", false, false, false),
    ("work_claim", false, false, false),
    ("work_accept", false, false, true),
    ("work_run", false, true, false),
    ("work_wait", true, false, true),
    ("work_status", true, false, true),
    ("work_cancel", false, true, false),
    ("work_finish", false, false, false),
    ("work_schedule", false, false, false),
    ("work_settings", false, false, true),
    ("work_plan", true, false, true),
    ("pool_define", false, false, true),
    ("pool_lease", false, false, false),
    ("pool_release", false, false, true),
    ("quiet_window", false, false, false),
    ("watchdog_report", true, false, true),
    ("reclaim", false, true, false),
    ("escalate", false, false, false),
    ("policy_set", false, false, true),
    ("metrics", true, false, true),
    ("card_update", false, false, true),
];

fn tool_json(name: &str) -> Value {
    list_json()["tools"]
        .as_array()
        .and_then(|a| a.iter().find(|t| t["name"] == name).cloned())
        .unwrap_or(Value::Null)
}

#[test]
fn table_matches_the_spec_order_and_annotations() {
    let got: Vec<_> = table()
        .iter()
        .map(|t| (t.name, t.read_only, t.destructive, t.idempotent))
        .collect();
    assert_eq!(got, SPEC.to_vec());
}

#[test]
fn tool_names_are_unique() {
    let names: HashSet<&str> = table().iter().map(|t| t.name).collect();
    assert_eq!(names.len(), table().len());
}

#[test]
fn every_tool_is_listed_with_an_object_schema_and_no_dollar_schema() {
    let list = list_json();
    let tools = list["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), SPEC.len());
    for t in tools {
        let name = &t["name"];
        assert!(
            t["description"].as_str().is_some_and(|d| !d.is_empty()),
            "{name}"
        );
        assert_eq!(t["inputSchema"]["type"], "object", "{name}");
        assert!(t["inputSchema"].get("$schema").is_none(), "{name}");
        assert_eq!(t["annotations"]["openWorldHint"], false, "{name}");
    }
}

#[test]
fn annotations_in_list_json_follow_the_table() {
    let read = tool_json("read");
    assert_eq!(read["annotations"]["readOnlyHint"], true);
    assert_eq!(read["annotations"]["title"], "Read a channel");

    assert_eq!(
        tool_json("channel_remove")["annotations"]["destructiveHint"],
        true
    );
    assert_eq!(tool_json("ack")["annotations"]["idempotentHint"], true);
    assert_eq!(tool_json("chat")["annotations"]["readOnlyHint"], false);
}

#[test]
fn parse_args_accepts_null_for_an_empty_args_type() {
    assert!(parse_args::<WhoamiArgs>(Value::Null).is_ok());
}

#[test]
fn parse_args_refuses_an_unknown_field() {
    let e = parse_args::<AckArgs>(json!({"channel": "c", "seq": 1, "extra": true}))
        .expect_err("unknown field");
    assert_eq!(e.code, "invalid_arguments");
    assert!(
        e.message.starts_with("invalid arguments: "),
        "{}",
        e.message
    );
}

#[test]
fn parse_args_refuses_a_missing_required_field() {
    let e = parse_args::<AckArgs>(json!({"channel": "c"})).expect_err("missing seq");
    assert_eq!(e.code, "invalid_arguments");
}

#[test]
fn result_json_wraps_a_success() {
    let r: ToolResult = Ok(ToolOutput {
        structured: json!({"seq": 3}),
        text: "posted".into(),
    });
    let v = result_json(&r);
    assert_eq!(v["content"][0]["type"], "text");
    assert_eq!(v["content"][0]["text"], "posted");
    assert_eq!(v["structuredContent"], json!({"seq": 3}));
    assert_eq!(v["isError"], false);
}

#[test]
fn result_json_wraps_a_non_object_structured_value() {
    let r: ToolResult = Ok(ToolOutput {
        structured: json!([1, 2]),
        text: String::new(),
    });
    assert_eq!(
        result_json(&r)["structuredContent"],
        json!({"value": [1, 2]})
    );
}

#[test]
fn result_json_reports_an_error_with_is_error() {
    let r: ToolResult = Err(ToolError {
        code: "usage".into(),
        message: "bad".into(),
    });
    let v = result_json(&r);
    assert_eq!(v["isError"], true);
    assert_eq!(v["content"][0]["text"], "error usage: bad");
    assert_eq!(
        v["structuredContent"],
        json!({"error": {"code": "usage", "message": "bad"}})
    );
}

#[test]
fn client_errors_keep_their_code() {
    let e = ToolError::from(ClientError::Usage("need a channel".into()));
    assert_eq!(e.code, "usage");
    assert_eq!(e.message, "need a channel");
    let e = ToolError::from(ClientError::UnexpectedReply);
    assert_eq!(e.code, "unexpected_reply");
}
