//! Brief CX-1: private roots never in tool output; snippet length limit; MCP over HTTP and the socket
//! protocol under bad input. A served ctxd with the fake embedder (fast, no model needed).

mod common;
use common::*;
use serde_json::{Value, json};

/// Words that only the private documents contain (queries may echo "Zebrafink", never these).
const SECRETS: &[&str] = &[
    "geheimprojekt",
    "streng vertraulich",
    "zebrafink-vereins",
    "zebrafink-konto",
    "zebrafink-notiz",
    "steuer-privat",
];

fn setup(name: &str, port: u16, extra: &str) -> (Served, std::path::PathBuf) {
    let d = tmp(name);
    let public = d.join("public");
    copy_dir(&eval_docs(), &public);
    let private = d.join("privat");
    std::fs::create_dir_all(&private).unwrap();
    std::fs::write(
        private.join("zebrafink-notiz.txt"),
        "Geheimprojekt Zebrafink\nDie Lohnsteuer des Zebrafink-Vereins 2025 ist streng vertraulich. Zweiter Satz.",
    )
    .unwrap();
    std::fs::write(
        private.join("steuer-privat.txt"),
        "Steuererklärung 2025 Zebrafink Anlage\nNur für Georg: Zebrafink-Konto bei der Bank.",
    )
    .unwrap();
    let cfg = write_config(&d, port, &[(&public, false), (&private, true)], extra);
    (serve(&d, &cfg, "fake", port), private)
}

fn assert_clean(body: &str, private: &std::path::Path) {
    let low = body.to_lowercase();
    for s in SECRETS {
        assert!(!low.contains(s), "private content {s:?} leaked to MCP: {body}");
    }
    assert!(!body.contains(private.to_str().unwrap()), "private root path leaked: {body}");
}

#[test]
fn private_roots_never_reach_claude() {
    let (s, private) = setup("privacy", 29541, "");
    let port = s.port;

    // The desktop (socket) sees the private documents.
    let v = sock_call(&s.sock, "context_search", json!({"query": "Zebrafink"})).unwrap();
    let items = v["result"]["items"].as_array().unwrap();
    assert!(!items.is_empty() && items[0]["path"].as_str().unwrap().contains("privat"), "{v}");
    let private_doc = items[0]["doc"].as_str().unwrap().to_string();

    // Claude does not, in any tool.
    let (b, r) = mcp_tool(port, "context_search", json!({"query": "Zebrafink"}));
    assert_clean(&b, &private);
    for it in r["structuredContent"]["items"].as_array().unwrap() {
        assert_ne!(it["doc"].as_str(), Some(private_doc.as_str()));
    }
    let (b, r) = mcp_tool(port, "context_search", json!({"query": "Steuererklärung 2025 Lohnsteuer", "limit": 500}));
    assert_clean(&b, &private);
    assert!(!r["structuredContent"]["items"].as_array().unwrap().is_empty());
    let (b, _) = mcp_tool(port, "context_status", json!({}));
    assert_clean(&b, &private);
    let (b, r) = mcp_tool(port, "context_explain", json!({"doc": private_doc, "query": "Zebrafink"}));
    assert_clean(&b, &private);
    assert_eq!(r["isError"], json!(true));
    let (b, r) = mcp_tool(port, "workspace_open", json!({"query": "Zebrafink Steuererklärung"}));
    assert_clean(&b, &private);
    let ws = r["structuredContent"]["workspace"]["id"].as_str().unwrap().to_string();
    let file = private.join("zebrafink-notiz.txt");
    let (b, r) = mcp_tool(
        port,
        "workspace_add",
        json!({"workspace": ws, "paths": [file.to_str().unwrap()], "docs": [private_doc]}),
    );
    assert!(r["structuredContent"]["added"].as_array().unwrap().is_empty(), "{b}");
    assert!(b.contains("not_found"));
    let (b, r) = mcp_tool(port, "dismiss", json!({"workspace": ws, "doc": private_doc}));
    assert_eq!(r["isError"], json!(true), "{b}");

    // A workspace Georg filled on the desktop with a private document: Claude sees the rest only.
    let v = sock_call(&s.sock, "workspace_open", json!({"name": "Steuer privat", "query": "Steuererklärung 2025"}))
        .unwrap();
    let ws2 = v["result"]["workspace"]["id"].as_str().unwrap().to_string();
    let v = sock_call(&s.sock, "workspace_add", json!({"workspace": ws2, "docs": [private_doc]})).unwrap();
    assert_eq!(v["result"]["added"].as_array().unwrap().len(), 1, "{v}");
    let v = sock_call(&s.sock, "workspace_open", json!({"workspace": ws2})).unwrap();
    assert!(v.to_string().to_lowercase().contains("zebrafink-notiz"), "desktop sees it");
    let (b, r) = mcp_tool(port, "workspace_open", json!({"workspace": ws2}));
    assert_clean(&b, &private);
    assert!(!r["structuredContent"]["workspace"]["groups"].as_array().unwrap().is_empty());
    let (b, _) = mcp_tool(port, "workspace_list", json!({}));
    assert_clean(&b, &private);
    // Opening it over MCP must not drop the private item for the desktop.
    let v = sock_call(&s.sock, "workspace_open", json!({"workspace": ws2})).unwrap();
    assert!(v.to_string().to_lowercase().contains("zebrafink-notiz"), "desktop still sees it after an MCP open");
    // Every workspace tool Claude has, on the workspace with the private item: nothing private comes back,
    // and Claude's edits keep the item for the desktop.
    let file = private.join("steuer-privat.txt");
    for (tool, args) in [
        ("workspace_add", json!({"workspace": ws2, "paths": [file.to_str().unwrap()], "docs": [private_doc]})),
        ("workspace_rename", json!({"workspace": ws2, "name": "Steuer 2025"})),
        ("workspace_pin", json!({"workspace": ws2, "pinned": true})),
        ("workspace_open", json!({"workspace": ws2})),
        ("workspace_open", json!({"name": "Steuer 2025"})),
        ("workspace_list", json!({"pinned_only": true})),
        ("workspace_list", json!({"query": "steuer"})),
    ] {
        let (b, mut r) = mcp_tool(port, tool, args);
        assert_ne!(r["isError"], json!(true), "{tool}: {b}");
        if tool == "workspace_add" {
            // `skipped` only echoes what Claude itself sent, answered with "not_found".
            let sc = &mut r["structuredContent"];
            assert!(sc["added"].as_array().unwrap().is_empty(), "{b}");
            assert!(sc["skipped"].as_array().unwrap().iter().all(|x| x["reason"] == "not_found"), "{b}");
            sc["skipped"] = json!([]);
            r["content"] = json!([]);
            assert_clean(&r.to_string(), &private);
        } else {
            assert_clean(&b, &private);
        }
    }
    let v = sock_call(&s.sock, "workspace_open", json!({"workspace": ws2})).unwrap();
    assert_eq!(v["result"]["workspace"]["name"], "Steuer 2025");
    assert!(v.to_string().to_lowercase().contains("zebrafink-notiz"), "desktop still sees it after MCP edits");

    // context_forget is delete_overwrite: refused over MCP without Georg's setting.
    let (b, r) = mcp_tool(port, "context_forget", json!({"scope": "all"}));
    assert_eq!(r["isError"], json!(true));
    assert!(b.contains("denied"));
}

/// With Georg's setting `context.claude.allow_forget`, Claude may forget, and still never reaches private data.
#[test]
fn forget_over_mcp_with_the_setting() {
    let (s, private) = setup("forget-mcp", 29546, "[context.claude]\nallow_forget = true\n");
    let port = s.port;
    let v = sock_call(&s.sock, "context_search", json!({"query": "Zebrafink"})).unwrap();
    let private_doc = v["result"]["items"][0]["doc"].as_str().unwrap().to_string();
    let (_, r) = mcp_tool(port, "context_search", json!({"query": "Mietvertrag"}));
    let public_doc = r["structuredContent"]["items"][0]["doc"].as_str().unwrap().to_string();

    // A private document cannot be forgotten by Claude (it does not exist for Claude).
    let (b, r) = mcp_tool(port, "context_forget", json!({"scope": "document", "doc": private_doc}));
    assert_eq!(r["isError"], json!(true), "{b}");
    assert!(b.contains("not_found"));
    assert_clean(&b, &private);
    let (b, r) = mcp_tool(port, "context_forget", json!({"scope": "document", "doc": public_doc}));
    assert_ne!(r["isError"], json!(true), "{b}");
    assert_clean(&b, &private);
    let (b, r) = mcp_tool(port, "context_forget", json!({"scope": "index"}));
    assert_ne!(r["isError"], json!(true), "{b}");
    assert!(r["structuredContent"]["files"].as_u64().unwrap() > 0, "{b}");
    assert_clean(&b, &private);
    // The index is rebuilt from the roots; the private document is back for the desktop only.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        let v = sock_call(&s.sock, "context_search", json!({"query": "Zebrafink"})).unwrap();
        if v["result"]["items"].as_array().is_some_and(|a| !a.is_empty()) {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "index not rebuilt after forget");
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let (b, _) = mcp_tool(port, "context_search", json!({"query": "Zebrafink"}));
    assert_clean(&b, &private);
    let (b, _) = mcp_tool(port, "context_status", json!({}));
    assert_clean(&b, &private);
}

#[test]
fn mcp_refuses_foreign_host_names() {
    let (s, _) = setup("host-header", 29547, "");
    let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}).to_string();
    // DNS rebinding: a page on evil.example resolved to 127.0.0.1 sends its own name as Host.
    assert_eq!(http(s.port, "POST", "/mcp", &[("Host", "evil.example:29547")], &init).0, 403);
    assert_eq!(http(s.port, "POST", "/mcp", &[("Host", "127.0.0.1:29547")], &init).0, 200);
}

#[test]
fn snippets_are_short() {
    let (s, _) = setup("snippets", 29542, "");
    for q in ["Steuererklärung 2025", "Mietvertrag und Nebenkosten", "Rezepte zum Nachkochen", "Urlaub in Italien"] {
        // Claude may ask for more, it never gets more than the setting (200).
        for asked in [json!(null), json!(5000), json!(50)] {
            let mut args = json!({"query": q, "limit": 20});
            if !asked.is_null() {
                args["snippet_chars"] = asked.clone();
            }
            let (_, r) = mcp_tool(s.port, "context_search", args);
            let max = asked.as_u64().map(|a| a.min(200)).unwrap_or(200) as usize;
            for it in r["structuredContent"]["items"].as_array().unwrap() {
                let sn = it["snippet"].as_str().unwrap_or("");
                assert!(sn.chars().count() <= max, "{} > {max}: {sn}", sn.chars().count());
                assert!(it.get("text").is_none(), "never full text");
            }
        }
    }
    drop(s);
    let (s, _) = setup("snippets-off", 29543, "[context.claude]\nsnippet_chars = 0\n");
    let (_, r) = mcp_tool(s.port, "context_search", json!({"query": "Steuererklärung 2025"}));
    let items = r["structuredContent"]["items"].as_array().unwrap();
    assert!(!items.is_empty());
    assert!(items.iter().all(|i| i.get("snippet").is_none()), "snippet_chars = 0 turns them off");
}

#[test]
fn mcp_http_and_socket_reject_bad_input() {
    let (s, _) = setup("protocol", 29544, "");
    let p = s.port;
    let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2099-01-01"}})
        .to_string();
    let (st, b) = http(p, "POST", "/mcp", &[], &init);
    assert_eq!(st, 200);
    assert_eq!(serde_json::from_str::<Value>(&b).unwrap()["result"]["protocolVersion"], "2099-01-01");
    let (st, b) = http(p, "POST", "/mcp", &[], &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}).to_string());
    assert_eq!(st, 200);
    let tools = serde_json::from_str::<Value>(&b).unwrap()["result"]["tools"].as_array().unwrap().len();
    assert_eq!(tools, 10);
    assert_eq!(
        http(p, "POST", "/mcp", &[], &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string()).0,
        202
    );
    assert_eq!(http(p, "POST", "/mcp", &[("Origin", "http://evil.example")], &init).0, 403);
    assert_eq!(http(p, "GET", "/mcp", &[], "").0, 405);
    assert_eq!(http(p, "POST", "/other", &[], &init).0, 404);
    let (st, b) = http(p, "POST", "/mcp", &[], "{not json");
    assert_eq!(st, 400);
    assert!(b.contains("-32700"));
    assert_eq!(http(p, "POST", "/mcp", &[], "[1,2]").0, 400);
    let big = "x".repeat(300 << 10);
    assert_eq!(http(p, "POST", "/mcp", &[], &big).0, 413);
    let (_, b) = http(
        p,
        "POST",
        "/mcp",
        &[],
        &json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "rm_rf", "arguments": {}}})
            .to_string(),
    );
    assert!(b.contains("-32602"));
    let (_, b) = http(p, "POST", "/mcp", &[], &json!({"jsonrpc": "2.0", "id": 4, "method": "nope"}).to_string());
    assert!(b.contains("-32601"));
    let (_, r) = mcp_tool(p, "context_search", json!({"query": 5}));
    assert_eq!(r["isError"], json!(true));
    let (_, r) = mcp_tool(p, "context_search", json!({"query": "x", "since": "gestern"}));
    assert_eq!(r["isError"], json!(true));

    // Socket: bad JSON, unknown op, a line over 1 MiB, then the same connection still answers.
    assert_eq!(sock_line(&s.sock, b"{oops")["error"]["code"], "bad_request");
    assert_eq!(sock_line(&s.sock, br#"{"id":1,"op":"format_disk"}"#)["error"]["code"], "bad_request");
    assert_eq!(sock_line(&s.sock, br#"{"id":1,"op":"search","args":"x"}"#)["error"]["code"], "bad_request");
    use std::io::{BufRead, BufReader, Write};
    let c = std::os::unix::net::UnixStream::connect(&s.sock).unwrap();
    let mut w = c.try_clone().unwrap();
    let mut r = BufReader::new(c);
    let mut long = vec![b'a'; (1 << 20) + 10];
    long.push(b'\n');
    w.write_all(&long).unwrap();
    w.write_all(b"{\"id\":9,\"op\":\"ping\"}\n").unwrap();
    let mut l = String::new();
    r.read_line(&mut l).unwrap();
    assert!(l.contains("bad_request"), "{l}");
    l.clear();
    r.read_line(&mut l).unwrap();
    assert!(l.contains("pong") && l.contains("\"id\":9"), "{l}");
}
