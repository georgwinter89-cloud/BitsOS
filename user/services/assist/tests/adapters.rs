//! Each adapter against its recorded streams (host/tests/cl/assist-rec, written by hand after the providers'
//! documented formats; no real provider was ever contacted): text only, text with two tool calls, a tool call
//! split across deltas, an error inside the stream, a cut stream, error statuses, and the request each sends.

mod common;
use assist::conv::{Conversation, Message, Part, Stop, ToolResult, Turn};
use assist::json::{Json, parse};
use assist::net::sse::Sse;
use assist::provider::{ApiError, ErrKind, Event, Kind, Place, Provider};
use assist::secret::Secret;
use assist::tools::{Class, ToolSpec};
use common::{KEY, recording};

fn provider(id: &str, kind: Kind, address: &str) -> Provider {
    Provider { id: id.into(), kind, address: address.into(), model: "claude-opus-5-5".into(), key: Secret::new(KEY), place: Place::Cloud, max_tokens: 64_000 }
}

fn tool() -> ToolSpec {
    ToolSpec {
        name: "settings_set".into(),
        description: "Change one setting.".into(),
        schema: parse(r#"{"type":"object","properties":{"key":{"type":"string"},"value":{"type":["boolean","integer","string"]}},"required":["key","value"]}"#).unwrap(),
        class: Class::Change,
    }
}

/// Runs a recording through the adapter in pieces of `step` bytes: the text deltas in order, the tool calls
/// as they completed, and the turn or the error.
fn play(kind: Kind, file: &str, step: usize) -> (String, Vec<String>, Result<Turn, ApiError>) {
    let p = provider("p1", kind, "https://example.invalid");
    let mut parser = kind.adapter().parser(&p);
    let mut sse = Sse::default();
    let (mut text, mut calls) = (String::new(), Vec::new());
    let mut sink = |e: Event| match e {
        Event::Text(t) => text.push_str(t),
        Event::Call(c) => calls.push(format!("{} {}", c.name, c.args.as_ref().map_or("INVALID".into(), Json::to_text))),
    };
    let data = recording(file);
    let mut failed = None;
    'feed: for piece in data.chunks(step).map(<[u8]>::to_vec).chain([Vec::new()]) {
        let events = if piece.is_empty() { sse.end() } else { sse.feed(&piece) };
        for ev in events {
            if let Err(e) = parser.event(&ev.event, &ev.data, &mut sink) {
                failed = Some(e);
                break 'feed;
            }
        }
        if parser.done() {
            break;
        }
    }
    let r = match failed {
        Some(e) => Err(e),
        None => parser.finish(),
    };
    (text, calls, r)
}

const KINDS: [(Kind, &str, &str); 3] = [(Kind::Anthropic, "anthropic", "ANTHROPIC"), (Kind::OpenAi, "openai", "OPENAI"), (Kind::Gemini, "gemini", "GEMINI")];

#[test]
fn text_only() {
    for (kind, name, mark) in KINDS {
        for step in [1, 7, 4096] {
            let (text, calls, turn) = play(kind, &format!("{name}-text.sse"), step);
            let turn = turn.unwrap_or_else(|e| panic!("{name} step {step}: {e:?}"));
            let want = format!("Hallo Georg, hier antwortet die Aufzeichnung {mark}-OK. Grüße mit Umlauten: äöüß.");
            assert_eq!(text, want, "{name} step {step}: the deltas in order");
            assert_eq!(turn.message.text(), want, "{name}");
            assert_eq!(turn.stop, Stop::End, "{name}");
            assert!(calls.is_empty() && turn.message.calls().is_empty(), "{name}");
        }
    }
}

#[test]
fn text_with_two_tool_calls_one_split_across_deltas() {
    for (kind, name, _) in KINDS {
        for step in [1, 11, 4096] {
            let (text, calls, turn) = play(kind, &format!("{name}-tools.sse"), step);
            let turn = turn.unwrap_or_else(|e| panic!("{name} step {step}: {e:?}"));
            assert_eq!(text, "Ich sehe nach und stelle es um.", "{name}");
            assert_eq!(calls, [r#"settings_get {"key":"theme.scale"}"#, r#"settings_set {"key":"theme.scale","value":150}"#], "{name} step {step}");
            assert_eq!(turn.stop, Stop::ToolUse, "{name}");
            let got = turn.message.calls();
            assert_eq!(got.len(), 2, "{name}");
            assert!(got[0].id != got[1].id && !got[0].id.is_empty(), "{name}: every call has its own id");
            assert_eq!(got[1].args.as_ref().unwrap().get("value"), Some(&Json::Int(150)), "{name}");
        }
    }
}

#[test]
fn a_cut_stream_is_an_error_after_its_text() {
    for (kind, name, _) in KINDS {
        for step in [1, 4096] {
            let (text, _, turn) = play(kind, &format!("{name}-cut.sse"), step);
            assert_eq!(text, "Dieser Satz wird mitten", "{name}: what arrived was shown");
            assert_eq!(turn.unwrap_err().kind, ErrKind::Cut, "{name} step {step}");
        }
    }
}

#[test]
fn an_error_inside_the_stream() {
    let (text, _, turn) = play(Kind::Anthropic, "anthropic-overloaded.sse", 9);
    let e = turn.unwrap_err();
    assert_eq!((text.as_str(), e.kind), ("Ein Anfang, dann", ErrKind::Overloaded));
    assert_eq!(e.raw, "overloaded_error: Overloaded");
    let (text, _, turn) = play(Kind::OpenAi, "openai-error.sse", 9);
    let e = turn.unwrap_err();
    assert_eq!((text.as_str(), e.kind), ("Ein Anfang, dann", ErrKind::Overloaded));
    let p = provider("p1", Kind::Gemini, "https://example.invalid");
    let mut parser = Kind::Gemini.adapter().parser(&p);
    let e = parser.event("", r#"{"error":{"code":503,"message":"The model is overloaded.","status":"UNAVAILABLE"}}"#, &mut |_| {}).unwrap_err();
    assert_eq!(e.kind, ErrKind::Overloaded);
    // A line that is not JSON is the provider's fault, not a crash.
    let mut parser = Kind::Anthropic.adapter().parser(&p);
    assert_eq!(parser.event("content_block_delta", "<html>", &mut |_| {}).unwrap_err().kind, ErrKind::Protocol);
}

#[test]
fn error_statuses_mean_the_same_for_every_kind() {
    let cases: [(Kind, u16, &str, ErrKind); 22] = [
        (Kind::Anthropic, 401, r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"},"request_id":"req_1"}"#, ErrKind::Key),
        (Kind::Anthropic, 403, r#"{"type":"error","error":{"type":"permission_error","message":"no"}}"#, ErrKind::Forbidden),
        (Kind::Anthropic, 402, r#"{"type":"error","error":{"type":"billing_error","message":"no credit"}}"#, ErrKind::Billing),
        (Kind::Anthropic, 404, r#"{"type":"error","error":{"type":"not_found_error","message":"model: claude-nope"}}"#, ErrKind::ModelNotFound),
        (Kind::Anthropic, 429, r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#, ErrKind::RateLimit),
        (Kind::Anthropic, 529, r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#, ErrKind::Overloaded),
        (Kind::Anthropic, 500, r#"{"type":"error","error":{"type":"api_error","message":"oops"}}"#, ErrKind::Server),
        (Kind::Anthropic, 413, r#"{"type":"error","error":{"type":"request_too_large","message":"big"}}"#, ErrKind::TooLarge),
        (Kind::Anthropic, 400, r#"{"type":"error","error":{"type":"invalid_request_error","message":"max_tokens: too large"}}"#, ErrKind::BadRequest),
        (Kind::Anthropic, 502, "<html>bad gateway</html>", ErrKind::Server),
        (Kind::OpenAi, 401, r#"{"error":{"message":"Incorrect API key provided","type":"invalid_request_error","param":null,"code":"invalid_api_key"}}"#, ErrKind::Key),
        (Kind::OpenAi, 429, r#"{"error":{"message":"Rate limit reached","type":"requests","code":"rate_limit_exceeded"}}"#, ErrKind::RateLimit),
        (Kind::OpenAi, 429, r#"{"error":{"message":"You exceeded your current quota","type":"insufficient_quota","code":"insufficient_quota"}}"#, ErrKind::Billing),
        (Kind::OpenAi, 404, r#"{"error":{"message":"The model `x` does not exist","type":"invalid_request_error","code":"model_not_found"}}"#, ErrKind::ModelNotFound),
        (Kind::OpenAi, 503, r#"{"error":{"message":"overloaded","type":"server_error","code":null}}"#, ErrKind::Overloaded),
        (Kind::OpenAi, 400, r#"{"error":{"message":"too long","type":"invalid_request_error","code":"context_length_exceeded"}}"#, ErrKind::TooLarge),
        (Kind::OpenAiCompatible, 500, r#"{"error":"model failed to load"}"#, ErrKind::Server),
        (Kind::Gemini, 400, r#"{"error":{"code":400,"message":"API key not valid. Please pass a valid API key.","status":"INVALID_ARGUMENT"}}"#, ErrKind::Key),
        (Kind::Gemini, 404, r#"{"error":{"code":404,"message":"models/x is not found","status":"NOT_FOUND"}}"#, ErrKind::ModelNotFound),
        (Kind::Gemini, 429, r#"{"error":{"code":429,"message":"quota","status":"RESOURCE_EXHAUSTED","details":[{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"34s"}]}}"#, ErrKind::RateLimit),
        (Kind::Gemini, 503, r#"{"error":{"code":503,"message":"overloaded","status":"UNAVAILABLE"}}"#, ErrKind::Overloaded),
        (Kind::Gemini, 400, r#"{"error":{"code":400,"message":"bad field","status":"INVALID_ARGUMENT"}}"#, ErrKind::BadRequest),
    ];
    for (kind, status, body, want) in cases {
        let e = kind.adapter().error(status, body);
        assert_eq!(e.kind, want, "{kind:?} {status} {body}");
        assert_eq!(e.status, Some(status));
        assert!(!e.raw.contains('\n'));
    }
    let e = Kind::Gemini.adapter().error(429, cases[19].2);
    assert_eq!(e.retry_after.map(|d| d.as_secs()), Some(34), "Gemini names its pause in the body");
}

fn conversation_after_a_tool_turn(kind: Kind, name: &str, provider_id: &str) -> Conversation {
    let (_, _, turn) = play(kind, &format!("{name}-tools.sse"), 4096);
    let mut turn = turn.unwrap();
    if let Some(raw) = &mut turn.message.raw {
        raw.provider = provider_id.to_string();
    }
    let calls: Vec<_> = turn.message.calls().into_iter().cloned().collect();
    let results = vec![
        ToolResult { id: calls[0].id.clone(), name: calls[0].name.clone(), content: r#"{"ok":true,"value":125}"#.into(), is_error: false },
        ToolResult { id: calls[1].id.clone(), name: calls[1].name.clone(), content: "bad_value: too large".into(), is_error: true },
    ];
    Conversation { system: "Sei kurz.".into(), messages: vec![Message::user("Mach die Schrift größer"), turn.message, Message::results(results)] }
}

#[test]
fn the_anthropic_request() {
    let p = provider("p1", Kind::Anthropic, "https://api.anthropic.com");
    let conv = conversation_after_a_tool_turn(Kind::Anthropic, "anthropic", "p1");
    let req = Kind::Anthropic.adapter().request(&p, &conv, &[tool()]);
    assert_eq!(req.path, "/v1/messages");
    let header = |n: &str| req.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(n)).map(|(_, v)| v.as_str());
    assert_eq!(header("x-api-key"), Some(KEY));
    assert_eq!(header("anthropic-version"), Some("2026-10-21"));
    assert_eq!(header("content-type"), Some("application/json"));
    assert!(!req.body.contains(KEY), "the key travels in the header only");
    let b = parse(&req.body).unwrap();
    assert_eq!((b.str("model"), b.get("max_tokens"), b.get("stream")), (Some("claude-opus-5-5"), Some(&Json::Int(64_000)), Some(&Json::Bool(true))));
    assert_eq!(b.str("system"), Some("Sei kurz."));
    // Current models refuse sampling parameters, thinking budgets and forced tool use.
    for gone in ["temperature", "top_p", "top_k", "thinking", "tool_choice"] {
        assert!(b.get(gone).is_none(), "{gone} must not be sent");
    }
    let Some(Json::Arr(tools)) = b.get("tools") else { panic!("tools") };
    assert_eq!((tools[0].str("name"), tools[0].get("eager_input_streaming")), (Some("settings_set"), Some(&Json::Bool(true))));
    assert_eq!(tools[0].get("input_schema"), Some(&tool().schema));
    let Some(Json::Arr(msgs)) = b.get("messages") else { panic!("messages") };
    assert_eq!(msgs.len(), 3);
    // The assistant turn goes back block for block: the reasoning block with its signature first.
    let Some(Json::Arr(blocks)) = msgs[1].get("content") else { panic!("content") };
    let types: Vec<&str> = blocks.iter().filter_map(|b| b.str("type")).collect();
    assert_eq!(types, ["thinking", "text", "tool_use", "tool_use"]);
    assert_eq!(blocks[0].str("signature"), Some("EqQBCkYrecordedSignatureTools=="));
    assert_eq!(blocks[3].get("input"), Some(&parse(r#"{"key":"theme.scale","value":150}"#).unwrap()));
    // Both results in one user message, the failed one marked.
    let Some(Json::Arr(results)) = msgs[2].get("content") else { panic!("results") };
    assert_eq!(results.len(), 2);
    assert_eq!((results[0].str("type"), results[0].str("tool_use_id"), results[0].get("is_error")), (Some("tool_result"), Some("toolu_rec_get"), None));
    assert_eq!((results[1].str("tool_use_id"), results[1].get("is_error")), (Some("toolu_rec_set"), Some(&Json::Bool(true))));

    // Behind any other address (a gateway, a test server) tool inputs stay buffered.
    let gateway = provider("p1", Kind::Anthropic, "https://gateway.example/anthropic");
    let b = parse(&Kind::Anthropic.adapter().request(&gateway, &conv, &[tool()]).body).unwrap();
    let Some(Json::Arr(tools)) = b.get("tools") else { panic!("tools") };
    assert!(tools[0].get("eager_input_streaming").is_none());

    // Another provider never gets this one's reasoning blocks: the turn is rebuilt from text and calls.
    let other = provider("p2", Kind::Anthropic, "https://api.anthropic.com");
    let b = parse(&Kind::Anthropic.adapter().request(&other, &conv, &[]).body).unwrap();
    assert!(!b.to_text().contains("signature") && b.get("tools").is_none());
    let Some(Json::Arr(msgs)) = b.get("messages") else { panic!("messages") };
    let Some(Json::Arr(blocks)) = msgs[1].get("content") else { panic!("content") };
    assert_eq!(blocks.iter().filter_map(|b| b.str("type")).collect::<Vec<_>>(), ["text", "tool_use", "tool_use"]);
}

#[test]
fn the_openai_request() {
    let p = provider("p1", Kind::OpenAi, "https://api.openai.com/v1");
    let conv = conversation_after_a_tool_turn(Kind::OpenAi, "openai", "p1");
    let req = Kind::OpenAi.adapter().request(&p, &conv, &[tool()]);
    assert_eq!(req.path, "/chat/completions");
    assert!(req.headers.iter().any(|(k, v)| k == "Authorization" && *v == format!("Bearer {KEY}")));
    assert!(!req.body.contains(KEY));
    let b = parse(&req.body).unwrap();
    assert_eq!((b.str("model"), b.get("stream")), (Some("claude-opus-5-5"), Some(&Json::Bool(true))));
    let Some(Json::Arr(msgs)) = b.get("messages") else { panic!("messages") };
    let roles: Vec<&str> = msgs.iter().filter_map(|m| m.str("role")).collect();
    assert_eq!(roles, ["system", "user", "assistant", "tool", "tool"]);
    let Some(Json::Arr(calls)) = msgs[2].get("tool_calls") else { panic!("tool_calls") };
    assert_eq!((calls[1].str("id"), calls[1].str("type")), (Some("call_rec_set"), Some("function")));
    // Arguments are JSON text here, not an object.
    assert_eq!(calls[1].get("function").and_then(|f| f.str("arguments")), Some(r#"{"key":"theme.scale","value":150}"#));
    assert_eq!((msgs[3].str("tool_call_id"), msgs[3].str("content")), (Some("call_rec_get"), Some(r#"{"ok":true,"value":125}"#)));
    assert_eq!(msgs[4].str("content"), Some("ERROR: bad_value: too large"));
    let Some(Json::Arr(tools)) = b.get("tools") else { panic!("tools") };
    assert_eq!(tools[0].get("function").and_then(|f| f.get("parameters")), Some(&tool().schema));

    // A local engine without a key gets no Authorization header at all.
    let mut local = provider("strata", Kind::OpenAiCompatible, "http://192.168.100.20:8080/v1");
    local.key = Secret::default();
    let req = Kind::OpenAiCompatible.adapter().request(&local, &conv, &[]);
    assert!(!req.headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("authorization")));
}

#[test]
fn the_gemini_request() {
    let mut p = provider("p1", Kind::Gemini, "https://generativelanguage.googleapis.com/v1beta");
    p.model = "gemini-rec/..?x".into();
    let conv = conversation_after_a_tool_turn(Kind::Gemini, "gemini", "p1");
    let req = Kind::Gemini.adapter().request(&p, &conv, &[tool()]);
    assert_eq!(req.path, "/models/gemini-rec%2F..%3Fx:streamGenerateContent?alt=sse", "the model name cannot leave its path segment");
    assert!(req.headers.iter().any(|(k, v)| k == "x-goog-api-key" && v == KEY));
    assert!(!req.body.contains(KEY) && !req.path.contains(KEY));
    let b = parse(&req.body).unwrap();
    assert_eq!(b.get("systemInstruction").and_then(|s| s.get("parts")).map(Json::to_text), Some(r#"[{"text":"Sei kurz."}]"#.to_string()));
    let Some(Json::Arr(contents)) = b.get("contents") else { panic!("contents") };
    assert_eq!(contents.iter().filter_map(|c| c.str("role")).collect::<Vec<_>>(), ["user", "model", "user"]);
    // The model's parts go back as they came: joined text, the call with its signature.
    let Some(Json::Arr(parts)) = contents[1].get("parts") else { panic!("parts") };
    assert_eq!(parts[0].str("text"), Some("Ich sehe nach und stelle es um."));
    assert_eq!(parts[1].str("thoughtSignature"), Some("recordedGeminiSignatureGet=="));
    assert_eq!(parts.len(), 3);
    let Some(Json::Arr(results)) = contents[2].get("parts") else { panic!("results") };
    let r0 = results[0].get("functionResponse").unwrap();
    assert_eq!((r0.str("name"), r0.get("response").and_then(|r| r.str("result"))), (Some("settings_get"), Some(r#"{"ok":true,"value":125}"#)));
    assert!(r0.get("id").is_none(), "an id this client made up stays here");
    assert_eq!(results[1].get("functionResponse").and_then(|f| f.get("response")).and_then(|r| r.str("error")), Some("bad_value: too large"));
    let decl = b.get("tools").map(Json::to_text).unwrap();
    assert!(decl.contains(r#""functionDeclarations":[{"name":"settings_set""#) && decl.contains("parametersJsonSchema"), "{decl}");
}

#[test]
fn a_tool_call_that_is_not_json_is_kept_as_text_and_never_parsed_loosely() {
    let p = provider("p1", Kind::Anthropic, "https://example.invalid");
    let mut parser = Kind::Anthropic.adapter().parser(&p);
    let mut calls = Vec::new();
    for (ev, data) in [
        ("content_block_start", r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_x","name":"settings_set","input":{}}}"#),
        ("content_block_delta", r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"key\": \"theme.scale\", \"value\": 15"}}"#),
        ("content_block_stop", r#"{"type":"content_block_stop","index":0}"#),
        ("message_delta", r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"}}"#),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ] {
        parser.event(ev, data, &mut |e| {
            if let Event::Call(c) = e {
                calls.push(c.clone());
            }
        }).unwrap();
    }
    let turn = parser.finish().unwrap();
    assert_eq!(turn.stop, Stop::MaxTokens);
    assert!(calls[0].args.is_none(), "a truncated input is not a smaller valid one");
    assert_eq!(calls[0].args_text, r#"{"key": "theme.scale", "value": 15"#);
    assert!(matches!(&turn.message.parts[0], Part::Call(c) if c.args.is_none()));
}

/// Runs `data` (a whole stream) through the OpenAI adapter.
fn play_openai(data: &str) -> (Vec<String>, Turn) {
    let p = provider("p1", Kind::OpenAiCompatible, "http://192.168.100.20:8080");
    let mut parser = Kind::OpenAiCompatible.adapter().parser(&p);
    let mut calls = Vec::new();
    let mut sink = |e: Event| {
        if let Event::Call(c) = e {
            calls.push(format!("{} {} {}", c.id, c.name, c.args.as_ref().map_or("INVALID".into(), Json::to_text)));
        }
    };
    let mut sse = Sse::default();
    let mut events = sse.feed(data.as_bytes());
    events.extend(sse.end());
    for ev in events {
        parser.event(&ev.event, &ev.data, &mut sink).unwrap();
    }
    (calls, parser.finish().unwrap())
}

#[test]
fn openai_tool_calls_without_a_number() {
    let chunk = |calls: &str, finish: &str| format!("data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"tool_calls\":[{calls}]}},\"finish_reason\":{finish}}}]}}\n\n");
    let get = r#"{"id":"c1","type":"function","function":{"name":"settings_get","arguments":"{\"key\":\"theme.scale\"}"}}"#;
    let set = r#"{"id":"c2","type":"function","function":{"name":"settings_set","arguments":"{\"key\":\"theme.scale\",\"value\":150}"}}"#;
    let want = [r#"c1 settings_get {"key":"theme.scale"}"#, r#"c2 settings_set {"key":"theme.scale","value":150}"#];
    // Some engines send each call whole, in a chunk of its own, without `index`.
    let (calls, turn) = play_openai(&format!("{}{}data: [DONE]\n\n", chunk(get, "null"), chunk(set, "\"tool_calls\"")));
    assert_eq!(calls, want);
    assert_eq!((turn.stop, turn.message.calls().len()), (Stop::ToolUse, 2));
    // Both in one chunk, and without ids: the second name starts the second call.
    let bare = |name: &str, a: &str| format!(r#"{{"function":{{"name":"{name}","arguments":"{a}"}}}}"#);
    let both = format!("{},{}", bare("settings_get", r#"{\"key\":\"a\"}"#), bare("settings_list", "{}"));
    let (calls, _) = play_openai(&format!("{}data: [DONE]\n\n", chunk(&both, "\"tool_calls\"")));
    assert_eq!(calls, [r#"call_0 settings_get {"key":"a"}"#, "call_1 settings_list {}"]);
    // One unnumbered call in pieces is still one call.
    let head = r#"{"id":"c9","type":"function","function":{"name":"settings_set","arguments":"{\"key\":"}}"#;
    let tail = r#"{"function":{"arguments":"\"theme.scale\",\"value\":150}"}}"#;
    let (calls, _) = play_openai(&format!("{}{}data: [DONE]\n\n", chunk(head, "null"), chunk(tail, "\"tool_calls\"")));
    assert_eq!(calls, [r#"c9 settings_set {"key":"theme.scale","value":150}"#]);
}
