use super::*;

fn j(t: &str) -> Json {
    Json::parse(t).unwrap()
}

#[test]
fn names() {
    assert!(service_name_ok("settingsd").is_ok());
    assert!(service_name_ok("svc-2_x").is_ok());
    for bad in ["", "Settings", "1svc", "a b", "x".repeat(33).as_str(), "sv\u{1}c"] {
        assert!(service_name_ok(bad).is_err(), "{bad:?}");
    }
    assert!(tool_name_ok("settings_get").is_ok());
    assert!(tool_name_ok("svc.status-2").is_ok());
    for bad in ["", "a b", "x/y", "ü", "app.player.play", "y".repeat(65).as_str()] {
        assert!(tool_name_ok(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn schemas() {
    assert!(check_schema(&j(r#"{"type":"object"}"#)).is_ok());
    assert!(check_schema(&j(r#"{"type":"object","properties":{"k":{"type":"string"}},"required":["k"]}"#)).is_ok());
    let err = |t: &str| check_schema(&j(t)).unwrap_err();
    assert_eq!(err("[]"), "inputSchema must be an object");
    assert_eq!(err(r#"{"type":"string"}"#), "inputSchema: type must be \"object\"");
    assert_eq!(err(r#"{"properties":{}}"#), "inputSchema: type must be \"object\"");
    assert_eq!(err(r#"{"type":"object","properties":[]}"#), "inputSchema: properties must be an object");
    assert_eq!(err(r#"{"type":"object","properties":{"k":3}}"#), "inputSchema: property \"k\" must be an object");
    assert_eq!(err(r#"{"type":"object","required":[1]}"#), "inputSchema: required must be an array of strings");
}

#[test]
fn tools_are_checked_whole() {
    let ok = j(r#"{"tools":[{"name":"a","description":"A.\nTwo lines","inputSchema":{"type":"object"},"safety":"read"},{"name":"b","inputSchema":{"type":"object"},"safety":"delete_overwrite"}]}"#);
    let t = parse_tools(&ok).unwrap();
    assert_eq!(t.len(), 2);
    assert_eq!(t[0].schema, r#"{"type":"object"}"#);
    assert_eq!(t[1].safety, Safety::DeleteOverwrite);
    assert_eq!(t[1].description, "");
    assert!(t[0].listed > t[0].schema.len());
    let err = |s: &str| parse_tools(&j(s)).unwrap_err();
    assert_eq!(err(r#"{"tools":{}}"#), "tools must be an array");
    assert_eq!(err(r#"{"tools":[{"inputSchema":{"type":"object"},"safety":"read"}]}"#), "tools[0]: a tool without a name (string)");
    assert!(err(r#"{"tools":[{"name":"a","inputSchema":{"type":"object"}}]}"#).contains("safety must be"));
    assert!(err(r#"{"tools":[{"name":"a","inputSchema":{"type":"object"},"safety":"maybe"}]}"#).contains("safety must be"));
    assert_eq!(err(r#"{"tools":[{"name":"a","safety":"read"}]}"#), "tools[0]: tool a: no inputSchema");
    assert!(err(r#"{"tools":[{"name":"a","description":"x\u0007","inputSchema":{"type":"object"},"safety":"read"}]}"#).contains("control"));
    assert_eq!(
        err(r#"{"tools":[{"name":"a","inputSchema":{"type":"object"},"safety":"read"},{"name":"a","inputSchema":{"type":"object"},"safety":"read"}]}"#),
        "tools[1]: a is named twice"
    );
    let long = alloc::format!(r#"{{"tools":[{{"name":"a","inputSchema":{{"type":"object","x":"{}"}},"safety":"read"}}]}}"#, "x".repeat(SCHEMA_MAX));
    assert!(err(&long).contains("longer than"));
    assert!(parse_tools(&j("{}")).unwrap().is_empty());
}

#[test]
fn results() {
    let r = result_of(&j(r#"{"id":1,"result":{"content":[{"type":"text","text":"hi"},{"type":"image","data":"iVBORw==","mimeType":"image/png"},{"type":"audio","data":"","mimeType":"audio/wav"}]}}"#), "s");
    let out = r.unwrap();
    assert!(matches!(&out[0], ToolOutput::Text(t) if t == "hi"));
    assert!(matches!(&out[1], ToolOutput::Png(p) if p == &[0x89, b'P', b'N', b'G']));
    assert!(matches!(&out[2], ToolOutput::Text(t) if t.contains("audio")));
    assert_eq!(result_of(&j(r#"{"id":1,"result":{"content":[{"type":"text","text":"no"}],"isError":true}}"#), "s").err(), Some(ToolError::Failed("no".into())));
    assert_eq!(result_of(&j(r#"{"id":1,"error":{"code":-32602,"message":"key?"}}"#), "s").err(), Some(ToolError::BadArgument("key?".into())));
    assert_eq!(result_of(&j(r#"{"id":1,"error":{"code":-1,"message":"broke"}}"#), "s").err(), Some(ToolError::Failed("s: broke".into())));
    for bad in [r#"{"id":1,"result":3}"#, r#"{"id":1,"result":{}}"#, r#"{"id":1,"result":{"content":[{"text":"x"}]}}"#, r#"{"id":1,"result":{"content":[{"type":"image","data":"%%","mimeType":"image/png"}]}}"#] {
        assert!(matches!(result_of(&j(bad), "s"), Err(ToolError::Failed(m)) if m.starts_with("s answered with")), "{bad}");
    }
}

#[test]
fn lines() {
    let c = call_line(7, "t\"x", &j(r#"{"a":[1,"b"]}"#), 9000);
    assert!(c.ends_with('\n'));
    let m = j(c.trim_end());
    assert_eq!(m.get("method").and_then(Json::as_str), Some("tools/call"));
    let p = m.get("params").unwrap();
    assert_eq!(p.get("name").and_then(Json::as_str), Some("t\"x"));
    assert_eq!(p.get("_meta").and_then(|x| x.get("timeout_ms")).and_then(Json::as_u64), Some(9000));
    assert_eq!(p.get("_meta").and_then(|x| x.get("progressToken")).and_then(Json::as_u64), Some(7));
    let k = j(cancel_line(7, "timeout").trim_end());
    assert_eq!(k.get("params").and_then(|x| x.get("requestId")).and_then(Json::as_u64), Some(7));
    assert_eq!(ok_line("\"a\"", &names_result("registered", &["x".into(), "y".into()])), "{\"jsonrpc\":\"2.0\",\"id\":\"a\",\"result\":{\"registered\":[\"x\",\"y\"]}}\n");
    assert!(j(err_line("3", -32602, "bad \"x\"").trim_end()).get("error").is_some());
}
