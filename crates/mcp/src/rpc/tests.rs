//! The protocol parts of V1's `mcp-json-test.mjs` and `mcp-test.mjs`, against `handle` directly.
extern crate std;
use super::*;
use crate::ToolResult;
use alloc::vec;
use std::string::ToString;

fn echo(ctx: &ToolContext, args: &Json) -> ToolResult {
    let ms = (ctx.deadline_ns - 1_000_000_000) / 1_000_000;
    Ok(vec![ToolOutput::Text(format!("{} {ms}", args.get("text").and_then(Json::as_str).unwrap_or("")))])
}
fn picture(_: &ToolContext, _: &Json) -> ToolResult {
    Ok(vec![ToolOutput::Text("shot".into()), ToolOutput::Png(vec![0x89, b'P', b'N', b'G']), ToolOutput::Json(Json::obj(vec![("a", Json::Num(1.0))]))])
}
fn failing(_: &ToolContext, args: &Json) -> ToolResult {
    match args.get("how").and_then(Json::as_str) {
        Some("bad") => Err(ToolError::BadArgument("how".into())),
        Some("refused") => Err(ToolError::Refused("not now".into())),
        Some("big") => Ok(vec![ToolOutput::Text("x".repeat(ANSWER_MAX + 1))]),
        Some("quotes") => Ok(vec![ToolOutput::Text("\"".repeat(ANSWER_MAX / 2 + 10))]),
        Some("controls") => Ok(vec![ToolOutput::Text("a\u{1}".repeat(ANSWER_MAX / 6)), ToolOutput::Text("second".into())]),
        Some("big_picture") => Ok(vec![ToolOutput::Text("x".into()), ToolOutput::Png(vec![0; ANSWER_MAX])]),
        Some("big_error") => Err(ToolError::Failed("e".repeat(ANSWER_MAX))),
        _ => Err(ToolError::Failed("it broke".into())),
    }
}

static TOOLS: [ToolEntry; 4] = [
    ToolEntry { name: "zz_echo", description: "Echo.", input_schema: r#"{"type":"object","properties":{"text":{"type":"string"},"timeout_ms":{"type":"integer"}},"required":["text"]}"#, owner: "MC", safety: Safety::Read, handler: echo },
    ToolEntry { name: "picture", description: "A PNG.", input_schema: r#"{"type":"object","properties":{}}"#, owner: "DP", safety: Safety::Change, handler: picture },
    ToolEntry { name: "slow", description: "Own default deadline.", input_schema: r#"{"type":"object","properties":{"text":{"type":"string"},"timeout_ms":{"type":"integer","default":30000}}}"#, owner: "MC", safety: Safety::Read, handler: echo },
    ToolEntry { name: "failing", description: "Fails.", input_schema: r#"{"type":"object","properties":{"how":{"type":"string"}}}"#, owner: "MC", safety: Safety::DeleteOverwrite, handler: failing },
];

const NOW: u64 = 1_000_000_000;

fn h(body: &str) -> Answer {
    handle(body.as_bytes(), NOW, &TOOLS)
}
fn hb(body: &[u8]) -> Answer {
    handle(body, NOW, &TOOLS)
}
fn rq(method: &str, params: Option<&str>, id: &str) -> String {
    match params {
        Some(p) => format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{p}}}"#),
        None => format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}"}}"#),
    }
}
fn ping(params: &str) -> String {
    rq("ping", Some(params), "1")
}
fn json(a: &Answer) -> Json {
    Json::parse(&a.body).unwrap_or_else(|_| panic!("not JSON: {}", a.body))
}
#[track_caller]
fn is_result(a: &Answer, id: &str) -> Json {
    assert_eq!(a.status, 200, "{}", a.body);
    assert!(a.body.starts_with(&format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":")), "{}", a.body);
    json(a).get("result").cloned().unwrap()
}
#[track_caller]
fn is_error(a: &Answer, status: u16, code: i32, id: &str) -> String {
    assert_eq!(a.status, status, "{}", a.body);
    assert!(a.body.starts_with(&format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"error\":")), "{}", a.body);
    let e = json(a).get("error").cloned().unwrap();
    assert_eq!(e.get("code").and_then(Json::as_i64), Some(code as i64), "{}", a.body);
    e.get("message").and_then(Json::as_str).unwrap().to_string()
}
#[track_caller]
fn parse_error(a: &Answer) {
    is_error(a, 400, -32700, "null");
}

#[test]
fn initialize_answers_our_version_to_everyone() {
    for v in [r#""2025-06-18""#, r#""2025-03-26""#, r#""2024-11-05""#, r#""2099-01-01""#, r#""2025-06-18 ""#, r#""""#, "20250618", "null"] {
        let r = is_result(&h(&rq("initialize", Some(&format!(r#"{{"protocolVersion":{v},"capabilities":{{}},"clientInfo":{{"name":"t","version":"1"}}}}"#)), "1")), "1");
        assert_eq!(r.get("protocolVersion").and_then(Json::as_str), Some(PROTOCOL));
        assert_eq!(r.get("capabilities").unwrap().to_string(), r#"{"tools":{"listChanged":false}}"#);
        assert_eq!(r.get("serverInfo").unwrap().get("name").and_then(Json::as_str), Some("BitsOS"));
        assert!(r.get("serverInfo").unwrap().get("version").and_then(Json::as_str).is_some());
        assert!(r.get("instructions").and_then(Json::as_str).unwrap().contains("BitsOS V2"));
        let Json::Obj(m) = &r else { panic!() };
        assert_eq!(m.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(), ["protocolVersion", "capabilities", "serverInfo", "instructions"]);
    }
    is_result(&h(&rq("initialize", None, "1")), "1");
    is_result(&h(&rq("initialize", Some("[]"), "1")), "1");
    assert_eq!(h(&rq("initialize", None, "1")).method, "initialize");
}

#[test]
fn notifications_and_client_answers() {
    for b in [
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":3,"reason":"x"}}"#,
        r#"{"method":"no/such","jsonrpc":"2.0"}"#,
        r#"{"jsonrpc":"2.0","id":1,"result":{}}"#,
        r#"{"jsonrpc":"2.0","id":"x","error":{"code":-1,"message":"no"}}"#,
    ] {
        let a = h(b);
        assert_eq!((a.status, a.body.as_str()), (202, ""), "{b}");
    }
    assert_eq!(h(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).method, "notifications/initialized");
    for b in [r#"{"jsonrpc":"1.0","method":"notifications/initialized"}"#, r#"{"method":"notifications/initialized"}"#, r#"{"jsonrpc":"2.0","method":7}"#, r#"{"jsonrpc":"2.0","id":1}"#, "{}"] {
        is_error(&h(b), 400, -32600, "null");
    }
    is_error(&h(&rq("notifications/initialized", None, "5")), 200, -32601, "5");
}

#[test]
fn ping_and_methods() {
    for p in [None, Some("{}"), Some(r#"{"_meta":{"progressToken":1}}"#)] {
        assert_eq!(is_result(&h(&rq("ping", p, "1")), "1").to_string(), "{}");
    }
    for m in ["foo/bar", "Ping", "ping ", "tools/List", ""] {
        assert!(is_error(&h(&rq(m, None, "5")), 200, -32601, "5").starts_with("Method not found"));
    }
    let long = "x".repeat(300);
    let msg = is_error(&h(&rq(&long, None, "5")), 200, -32601, "5");
    assert!(msg.len() < 100, "names are cut for messages and the log");
}

#[test]
fn envelope_rules() {
    for b in [format!("[{}]", rq("ping", None, "1")), "[]".into(), format!("[{},{}]", rq("ping", None, "1"), rq("tools/list", None, "2"))] {
        assert_eq!(is_error(&h(&b), 400, -32600, "null"), "Batches are not supported");
    }
    for b in ["5", "\"ping\"", "null", "true", "-1.5e3"] {
        is_error(&h(b), 400, -32600, "null");
    }
    for b in [r#"{"id":3,"method":"ping"}"#, r#"{"jsonrpc":"1.0","id":3,"method":"ping"}"#, r#"{"jsonrpc":2.0,"id":3,"method":"ping"}"#, r#"{"jsonrpc":"2.0 ","id":3,"method":"ping"}"#, r#"{"jsonrpc":"2.0","id":3,"method":["ping"]}"#, r#"{"jsonrpc":"2.0","id":3,"method":null}"#] {
        is_error(&h(b), 400, -32600, "3");
    }
}

#[test]
fn ids_come_back_as_they_came() {
    let x254 = format!("\"{}\"", "x".repeat(254));
    for id in ["0", "1", "-1", "42", "9223372036854775807", "-9223372036854775808", "-0", "\"abc\"", "\"\"", r#""a\"b\\cä😀 ä😀""#, &x254] {
        is_result(&h(&format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"ping"}}"#)), id);
    }
    is_result(&h("{\"jsonrpc\":\"2.0\",\"id\": \t-17\n,\"method\":\"ping\"}"), "-17");
    let x255 = format!("\"{}\"", "x".repeat(255));
    for id in ["null", "1.5", "1e3", "1E0", "1.0", "true", "{}", "[]", "[1]", "9223372036854775808", "-9223372036854775809", "18446744073709551616", "123456789012345678901234567890", &x255] {
        is_error(&h(&format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"ping"}}"#)), 400, -32600, "null");
    }
}

#[test]
fn equal_keys_escapes_whitespace() {
    is_result(&h(r#"{"jsonrpc":"2.0","id":1,"method":"nope","method":"ping"}"#), "1");
    is_result(&h(r#"{"jsonrpc":"2.0","id":1,"id":2,"method":"ping"}"#), "2");
    is_result(&h(r#"{"jsonrpc":"1.0","id":1,"jsonrpc":"2.0","method":"ping"}"#), "1");
    is_error(&h(r#"{"jsonrpc":"2.0","id":1,"method":"ping","method":"nope"}"#), 200, -32601, "1");
    is_result(&h(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#), "1");
    let r = is_result(&h(r#"{"jsonrpc":"2.0","id":1,"method\u0000":"ping","method":"tools/list"}"#), "1");
    assert_eq!(r.get("tools").and_then(Json::as_array).map(|t| t.len()), Some(4));
    is_result(&h(" \r\n\t{ \"x\" : [ 1 , { } ] , \"jsonrpc\" : \"2.0\" , \"method\" : \"ping\" , \"id\" : 9 , \"y\" : null } \n"), "9");
}

#[test]
fn what_is_no_json() {
    let valid = r#"{"jsonrpc":"2.0","id":-7,"method":"ping","params":{"a":[[1,2],{"b":"😀\n"}],"c":1E+2}}"#;
    is_result(&h(valid), "-7");
    for n in 0..valid.len() {
        parse_error(&hb(&valid.as_bytes()[..n]));
    }
    for b in ["", " ", "\r\n\t ", "\u{a0}{}", "\u{feff}{}"] {
        parse_error(&h(b));
    }
    let in_string = |b: &[u8]| {
        let mut v = br#"{"jsonrpc":"2.0","id":1,"method":"ping","params":{"s":""#.to_vec();
        v.extend_from_slice(b);
        v.extend_from_slice(br#""}}"#);
        hb(&v)
    };
    for b in [&[0x80u8][..], &[0xBF], &[0xC0, 0x80], &[0xC1, 0xBF], &[0xC2], &[0xC3, 0x28], &[0xE0, 0x80, 0x80], &[0xED, 0xA0, 0x80], &[0xF4, 0x90, 0x80, 0x80], &[0xF8, 0x88, 0x80, 0x80, 0x80], &[0xFE], &[0xFF], &[0xF0, 0x9F, 0x98]] {
        parse_error(&in_string(b));
    }
    for b in [&[0xC2u8, 0x80][..], &[0xDF, 0xBF], &[0xE0, 0xA0, 0x80], &[0xED, 0x9F, 0xBF], &[0xEF, 0xBF, 0xBF], &[0xF0, 0x90, 0x80, 0x80], &[0xF4, 0x8F, 0xBF, 0xBF], &[0x7F]] {
        is_result(&in_string(b), "1");
    }
    let mut outside = rq("ping", None, "1").into_bytes();
    outside.extend_from_slice(&[0xC3, 0xA4]);
    parse_error(&hb(&outside));
    for e in [r"\x41", r"\u12", r"\uD800", r"\uDC00", r"\'", r"\a", r"\0"] {
        parse_error(&in_string(e.as_bytes()));
    }
    for c in [0u8, 1, 9, 10, 13, 0x1F] {
        parse_error(&in_string(&[b'a', c, b'b']));
    }
    for p in ["{\"n\":01}", "{\"n\":.5}", "{\"n\":NaN}", "{\"w\":tru}", "{\"w\":nulll}", "{\"a\"}", "{\"a\":1,}", "[1,]", "{'a':1}", "{a:1}", "{\"a\":1}}"] {
        parse_error(&h(&ping(p)));
    }
    for tail in [" x", ",", "]", "\u{0}"] {
        parse_error(&h(&format!("{}{tail}", rq("ping", None, "1"))));
    }
    parse_error(&h(&format!("{}{}", rq("ping", None, "1"), rq("ping", None, "1"))));
}

#[test]
fn nesting_and_size() {
    let deep = |n: usize| ping(&format!("{{\"a\":{}{}}}", "[".repeat(n), "]".repeat(n)));
    is_result(&h(&deep(62)), "1");
    parse_error(&h(&deep(63)));
    is_result(&h(&ping(&format!("{{\"a\":{}1{}}}", "{\"b\":".repeat(62), "}".repeat(62)))), "1");
    parse_error(&h(&"[".repeat(100_000)));
    is_result(&h(&ping(&format!("{{\"s\":\"{}\"}}", "ä".repeat(120_000)))), "1");
    is_result(&h(&ping(&format!("[{}]", ["0"; 60_000].join(",")))), "1");
}

#[test]
fn tools_list_from_the_registry() {
    let r = is_result(&h(&rq("tools/list", Some(r#"{"cursor":"x"}"#), "1")), "1");
    let tools = r.get("tools").and_then(Json::as_array).unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t.get("name").and_then(Json::as_str).unwrap()).collect();
    assert_eq!(names, ["failing", "picture", "slow", "zz_echo"], "sorted by name");
    for t in tools {
        assert!(t.get("description").and_then(Json::as_str).is_some());
        assert_eq!(t.get("inputSchema").unwrap().get("type").and_then(Json::as_str), Some("object"));
    }
    let ann = |i: usize| tools[i].get("annotations").unwrap().clone();
    assert_eq!(ann(0).to_string(), r#"{"readOnlyHint":false,"destructiveHint":true,"owner":"MC","safety":"delete_overwrite"}"#);
    assert_eq!(ann(1).get("safety").and_then(Json::as_str), Some("change"));
    assert_eq!(ann(3).get("readOnlyHint").and_then(Json::as_bool), Some(true));
}

fn call(name: &str, args: &str) -> Answer {
    h(&rq("tools/call", Some(&format!(r#"{{"name":"{name}","arguments":{args}}}"#)), "1"))
}
fn content(a: &Answer) -> (bool, Vec<Json>) {
    let r = is_result(a, "1");
    (r.get("isError").and_then(Json::as_bool).unwrap(), r.get("content").and_then(Json::as_array).unwrap().to_vec())
}

#[test]
fn tools_call_dispatch() {
    let a = call("zz_echo", r#"{"text":"Grüße 😀"}"#);
    let (err, c) = content(&a);
    assert!(!err && a.tool == "zz_echo" && a.method == "tools/call");
    assert_eq!(c[0].get("text").and_then(Json::as_str), Some("Grüße 😀 10000"), "default deadline 10 s");
    let (_, c) = content(&call("zz_echo", r#"{"text":"t","timeout_ms":500}"#));
    assert_eq!(c[0].get("text").and_then(Json::as_str), Some("t 500"));
    // A tool's schema may give its own default deadline; timeout_ms still wins.
    let (_, c) = content(&call("slow", r#"{"text":"s"}"#));
    assert_eq!(c[0].get("text").and_then(Json::as_str), Some("s 30000"));
    let (_, c) = content(&call("slow", r#"{"text":"s","timeout_ms":700}"#));
    assert_eq!(c[0].get("text").and_then(Json::as_str), Some("s 700"));
    let (err, c) = content(&call("picture", "{}"));
    assert!(!err);
    assert_eq!(c[1].to_string(), r#"{"type":"image","data":"iVBORw==","mimeType":"image/png"}"#);
    assert_eq!(c[2].get("text").and_then(Json::as_str), Some(r#"{"a":1}"#));
    let a = h(&rq("tools/call", Some(r#"{"name":"picture"}"#), "1"));
    assert!(!content(&a).0, "no arguments at all is fine");
}

#[test]
fn tools_call_errors() {
    let m = is_error(&call("no_such_tool", "{}"), 200, -32602, "1");
    assert!(m.starts_with("Unknown tool: no_such_tool ("), "{m}");
    for p in ["null", "{}", r#"{"name":5}"#, "[]"] {
        let m = is_error(&h(&rq("tools/call", Some(p), "1")), 200, -32602, "1");
        assert_eq!(m, "tools/call needs params with a tool name");
    }
    is_error(&h(&rq("tools/call", None, "1")), 200, -32602, "1");
    assert_eq!(is_error(&call("zz_echo", "[1]"), 200, -32602, "1"), "arguments must be an object");
    assert_eq!(is_error(&call("zz_echo", "{}"), 200, -32602, "1"), "Missing argument: text");
    assert_eq!(is_error(&call("zz_echo", r#"{"text":5}"#), 200, -32602, "1"), "Wrong type for argument: text");
    assert_eq!(is_error(&call("failing", r#"{"how":"bad"}"#), 200, -32602, "1"), "Bad argument: how");
    let (err, c) = content(&call("failing", "{}"));
    assert!(err && c[0].get("text").and_then(Json::as_str) == Some("ERROR: it broke"));
    let (err, c) = content(&call("failing", r#"{"how":"refused"}"#));
    assert!(err && c[0].get("text").and_then(Json::as_str) == Some("Refused: not now"));
    // Text above 4 MB is cut at the end with a note (the start is what a log or a program's output needs);
    // escaping counts (quotes double, control bytes take six).
    for (how, unit) in [("big", "x"), ("quotes", "\""), ("controls", "a\u{1}")] {
        let a = call("failing", &format!(r#"{{"how":"{how}"}}"#));
        let (err, c) = content(&a);
        let t = c[0].get("text").and_then(Json::as_str).unwrap();
        assert!(!err && a.body.len() <= ANSWER_MAX && a.body.len() > ANSWER_MAX - 4096, "{how}: {} bytes", a.body.len());
        assert!(t.starts_with(&unit.repeat(100)) && t.contains("[... cut: the last ") && t.ends_with("BitsOS answers are at most 4 MB]"), "{how}");
        if how == "controls" {
            assert!(c[1].get("text").and_then(Json::as_str).unwrap().starts_with("\n[... cut: the last 6 bytes"), "a later text gets only the note");
        }
    }
    let (err, c) = content(&call("failing", r#"{"how":"big_error"}"#));
    let t = c[0].get("text").and_then(Json::as_str).unwrap();
    assert!(err && t.starts_with("ERROR: eee") && t.contains("[... cut:"), "an error text is cut too");
    // A picture cannot be cut.
    let a = call("failing", r#"{"how":"big_picture"}"#);
    let (err, c) = content(&a);
    assert!(err && c[0].get("text").and_then(Json::as_str) == Some(TOO_BIG) && a.body.len() < 1000);
    for t in ["0", "-1", "1.5", "\"10\"", "86400001"] {
        let a = call("zz_echo", &format!(r#"{{"text":"x","timeout_ms":{t}}}"#));
        if t == "1.5" || t == "\"10\"" {
            is_error(&a, 200, -32602, "1");
        } else {
            let (err, c) = content(&a);
            assert!(err && c[0].get("text").and_then(Json::as_str).unwrap().contains("timeout_ms must be"), "{t}");
        }
    }
}

#[test]
fn escaped_len_matches_escape_into() {
    let mut chars: Vec<char> = (0u32..0x800).filter_map(char::from_u32).collect();
    chars.extend(['\u{ffff}', '\u{fffd}', '😀', '\u{10ffff}']);
    for c in chars {
        let mut s = String::new();
        json::escape_into(&c.to_string(), &mut s);
        assert_eq!(s.len() - 2, escaped_len(c), "{:?}", c);
    }
}
