//! MCP over HTTP on 127.0.0.1 (DESIGN §2, `spec/net.md` §5.1 as V1/V2): `POST /mcp`, JSON-RPC 2.0, answers
//! `application/json`; `initialize`, `notifications/*` (202), `ping`, `tools/list`, `tools/call`. No SSE, no
//! sessions, no batches. An `Origin` header is refused (no browser access); an optional bearer token.
//! Every tool call is `Caller::Mcp`: Claude never sees private roots and gets short snippets.

use crate::api;
use crate::engine::{Caller, Engine};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

const MAX_HEAD: usize = 16 << 10;
const MAX_BODY: usize = 256 << 10;

fn tool(name: &str, desc: &str, schema: Value, read_only: bool, destructive: bool) -> Value {
    json!({
        "name": name, "description": desc, "inputSchema": schema,
        "annotations": {"readOnlyHint": read_only, "destructiveHint": destructive, "openWorldHint": false},
    })
}

pub fn tools() -> Vec<Value> {
    let s = |props: Value, req: &[&str]| json!({"type": "object", "properties": props, "required": req});
    vec![
        tool(
            "context_search",
            "Search Georg's documents by meaning and keywords (German and English). Suche in Georgs Dokumenten nach Bedeutung und Stichwörtern. Returns title, path, kind, date, score, a short snippet and why each result is there.",
            s(
                json!({
                    "query": {"type": "string", "description": "What to look for, e.g. 'Steuererklärung 2025'"},
                    "limit": {"type": "integer", "minimum": 1, "default": 10},
                    "kinds": {"type": "array", "items": {"type": "string", "enum": ["text", "md", "html", "mail", "pdf", "docx", "odt", "other"]}},
                    "roots": {"type": "array", "items": {"type": "string"}},
                    "since": {"type": "string", "description": "YYYY-MM-DD: only documents changed since then"},
                    "explain": {"type": "boolean", "default": true},
                    "snippets": {"type": "boolean", "default": true},
                }),
                &["query"],
            ),
            true,
            false,
        ),
        tool(
            "workspace_open",
            "Open a workspace by id or name, or create one from a search (name or query). Arbeitsbereich öffnen oder aus einer Suche anlegen. Returns the items grouped (Briefe, Belege, Formulare, Mails, ...).",
            s(
                json!({
                    "workspace": {"type": "string"}, "name": {"type": "string"}, "query": {"type": "string"},
                    "limit": {"type": "integer", "minimum": 1, "default": 20},
                }),
                &[],
            ),
            false,
            false,
        ),
        tool(
            "workspace_add",
            "Add documents (by doc id or path) to a workspace. Dokumente zu einem Arbeitsbereich hinzufügen.",
            s(
                json!({"workspace": {"type": "string"}, "docs": {"type": "array", "items": {"type": "string"}},
                     "paths": {"type": "array", "items": {"type": "string"}}}),
                &["workspace"],
            ),
            false,
            false,
        ),
        tool(
            "workspace_list",
            "List workspaces. Arbeitsbereiche auflisten.",
            s(json!({"pinned_only": {"type": "boolean"}, "query": {"type": "string"}}), &[]),
            true,
            false,
        ),
        tool(
            "workspace_rename",
            "Rename a workspace. Arbeitsbereich umbenennen.",
            s(json!({"workspace": {"type": "string"}, "name": {"type": "string"}}), &["workspace", "name"]),
            false,
            false,
        ),
        tool(
            "workspace_pin",
            "Pin or unpin a workspace. Arbeitsbereich anheften oder lösen.",
            s(json!({"workspace": {"type": "string"}, "pinned": {"type": "boolean", "default": true}}), &["workspace"]),
            false,
            false,
        ),
        tool(
            "dismiss",
            "Hide a document in a workspace (undo with undo=true). Dokument im Arbeitsbereich ausblenden.",
            s(
                json!({"workspace": {"type": "string"}, "doc": {"type": "string"}, "scope": {"type": "string", "enum": ["workspace"]},
                     "undo": {"type": "boolean"}}),
                &["workspace", "doc"],
            ),
            false,
            false,
        ),
        tool(
            "context_status",
            "State of the context service: roots, counts, failed documents, embedder, indexing progress. Zustand des Kontextdienstes.",
            s(json!({}), &[]),
            true,
            false,
        ),
        tool(
            "context_explain",
            "Full ranking breakdown of one document for a query (why is this here). Warum steht das da: alle Zahlen.",
            s(json!({"doc": {"type": "string"}, "query": {"type": "string"}}), &["doc", "query"]),
            true,
            false,
        ),
        tool(
            "context_forget",
            "Forget: a document's cached text, a workspace, the usage log, the index or everything (delete_overwrite; needs Georg's OK). Vergessen.",
            s(
                json!({"scope": {"type": "string", "enum": ["document", "workspace", "usage", "index", "all"]},
                     "doc": {"type": "string"}, "workspace": {"type": "string"}}),
                &["scope"],
            ),
            false,
            true,
        ),
    ]
}

fn rpc_error(id: Value, code: i64, msg: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": msg}})
}

/// One JSON-RPC message → the answer (None for notifications).
pub fn handle_rpc(e: &Engine, req: &Value) -> Option<Value> {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let Some(id) = id.filter(|i| !i.is_null()) else {
        // Notifications get no answer (HTTP 202).
        return None;
    };
    if req.get("jsonrpc").and_then(Value::as_str) != Some("2.0") || method.is_empty() {
        return Some(rpc_error(id, -32600, "invalid request"));
    }
    let params = req.get("params").cloned().unwrap_or(json!({}));
    let result = match method {
        "initialize" => json!({
            "protocolVersion": params.get("protocolVersion").and_then(Value::as_str).unwrap_or("2025-06-18"),
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": "ctxd", "version": env!("CARGO_PKG_VERSION")},
            "instructions": "BitsOS context service (prototype): search Georg's documents and build workspaces. Results carry title, path and a short snippet only; private roots are never shown.",
        }),
        "ping" => json!({}),
        "tools/list" => json!({"tools": tools()}),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            if api::canonical(name) != Some(name) {
                return Some(rpc_error(id, -32602, &format!("unknown tool {:?}", crate::util::cut_bytes(name, 64))));
            }
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            match api::call(e, name, &args, Caller::Mcp) {
                Ok(v) => json!({
                    "content": [{"type": "text", "text": serde_json::to_string_pretty(&v).unwrap_or_default()}],
                    "structuredContent": v, "isError": false,
                }),
                Err(x) => json!({
                    "content": [{"type": "text", "text": format!("{}: {}", x.code, x.message)}],
                    "isError": true,
                }),
            }
        }
        _ => return Some(rpc_error(id, -32601, "method not found")),
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

fn respond(w: &mut TcpStream, code: u16, reason: &str, body: &[u8], close: bool) -> std::io::Result<()> {
    let ct = if body.is_empty() { "" } else { "Content-Type: application/json\r\n" };
    let conn = if close { "Connection: close\r\n" } else { "" };
    write!(w, "HTTP/1.1 {code} {reason}\r\n{ct}Content-Length: {}\r\n{conn}\r\n", body.len())?;
    w.write_all(body)?;
    w.flush()
}

struct Head {
    method: String,
    path: String,
    len: Option<usize>,
    origin: bool,
    host: Option<String>,
    auth: Option<String>,
    close: bool,
}

fn read_head(r: &mut BufReader<TcpStream>) -> Result<Option<Head>, u16> {
    let mut total = 0;
    let mut line = String::new();
    let mut lines = Vec::new();
    loop {
        line.clear();
        let n = r.by_ref().take((MAX_HEAD - total + 1) as u64).read_line(&mut line).map_err(|_| 408u16)?;
        if n == 0 {
            return if lines.is_empty() { Ok(None) } else { Err(400) };
        }
        total += n;
        if total > MAX_HEAD {
            return Err(431);
        }
        let l = line.trim_end_matches(['\r', '\n']).to_string();
        if l.is_empty() {
            if lines.is_empty() {
                continue;
            }
            break;
        }
        lines.push(l);
    }
    let mut first = lines[0].split(' ');
    let (method, path) = (first.next().unwrap_or("").to_string(), first.next().unwrap_or("").to_string());
    let mut h = Head { method, path, len: None, origin: false, host: None, auth: None, close: false };
    for l in &lines[1..] {
        let Some((k, v)) = l.split_once(':') else { continue };
        let v = v.trim();
        match k.trim().to_ascii_lowercase().as_str() {
            "content-length" => h.len = Some(v.parse().map_err(|_| 400u16)?),
            "origin" => h.origin = true,
            "host" => h.host = Some(v.to_ascii_lowercase()),
            "authorization" => h.auth = Some(v.to_string()),
            "connection" => h.close = v.eq_ignore_ascii_case("close"),
            _ => {}
        }
    }
    Ok(Some(h))
}

/// `Host` must name the loopback listener: 127.0.0.1, localhost or [::1], with our port or none.
fn host_ok(host: Option<&str>, port: u16) -> bool {
    let Some(h) = host else { return false };
    let (name, p) = match h.rsplit_once(':') {
        Some((n, p)) if !n.ends_with(':') && !p.contains(']') => (n, Some(p)),
        _ => (h, None),
    };
    matches!(name, "127.0.0.1" | "localhost" | "[::1]") && p.is_none_or(|p| p == port.to_string())
}

fn serve_conn(e: Arc<Engine>, s: TcpStream) {
    let port = s.local_addr().map(|a| a.port()).unwrap_or(0);
    let _ = s.set_read_timeout(Some(Duration::from_secs(60)));
    let _ = s.set_write_timeout(Some(Duration::from_secs(30)));
    let Ok(mut w) = s.try_clone() else { return };
    let mut r = BufReader::new(s);
    loop {
        let head = match read_head(&mut r) {
            Ok(Some(h)) => h,
            Ok(None) => return,
            Err(code) => {
                let _ = respond(&mut w, code, "Bad Request", b"", true);
                return;
            }
        };
        let fail = |w: &mut TcpStream, code: u16, reason: &str| {
            let _ = respond(w, code, reason, b"", true);
        };
        // A browser page (Origin) and a DNS-rebound name (Host other than the loopback address we listen on)
        // are refused before anything else is read.
        if head.origin || !host_ok(head.host.as_deref(), port) {
            return fail(&mut w, 403, "Forbidden");
        }
        if let Some(tok) = &e.cfg.mcp.token
            && head.auth.as_deref() != Some(&format!("Bearer {tok}"))
        {
            return fail(&mut w, 401, "Unauthorized");
        }
        if head.path != "/mcp" {
            return fail(&mut w, 404, "Not Found");
        }
        if head.method != "POST" {
            return fail(&mut w, 405, "Method Not Allowed");
        }
        let Some(len) = head.len else { return fail(&mut w, 411, "Length Required") };
        if len > MAX_BODY {
            // Read (and drop) what the client is still sending, bounded: closing with unread data makes the
            // kernel send a reset that can swallow the 413 before the client reads it.
            let _ = r.get_ref().set_read_timeout(Some(Duration::from_secs(1)));
            let _ = std::io::copy(&mut r.by_ref().take(len.min(16 * MAX_BODY) as u64), &mut std::io::sink());
            return fail(&mut w, 413, "Payload Too Large");
        }
        let mut body = vec![0u8; len];
        if r.read_exact(&mut body).is_err() {
            return fail(&mut w, 408, "Request Timeout");
        }
        let req: Value = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(_) => {
                let b = rpc_error(Value::Null, -32700, "parse error").to_string();
                let _ = respond(&mut w, 400, "Bad Request", b.as_bytes(), true);
                return;
            }
        };
        if !req.is_object() {
            let b = rpc_error(Value::Null, -32600, "batches are not supported").to_string();
            let _ = respond(&mut w, 400, "Bad Request", b.as_bytes(), true);
            return;
        }
        let ok = match handle_rpc(&e, &req) {
            Some(v) => respond(&mut w, 200, "OK", v.to_string().as_bytes(), head.close),
            None => respond(&mut w, 202, "Accepted", b"", head.close),
        };
        if ok.is_err() || head.close {
            return;
        }
    }
}

pub fn bind(port: u16) -> Result<TcpListener, String> {
    TcpListener::bind(("127.0.0.1", port)).map_err(|e| format!("mcp 127.0.0.1:{port}: {e}"))
}

pub fn serve(e: Arc<Engine>, l: TcpListener) {
    for s in l.incoming() {
        let Ok(s) = s else { continue };
        // The listener is on 127.0.0.1; anything else is refused anyway.
        if !s.peer_addr().is_ok_and(|a| a.ip().is_loopback()) {
            continue;
        }
        let e = Arc::clone(&e);
        let _ = std::thread::Builder::new().name("ctxd-mcp".into()).spawn(move || serve_conn(e, s));
    }
}

#[cfg(test)]
mod tests {
    use super::host_ok;

    #[test]
    fn only_loopback_hosts() {
        for h in ["localhost", "127.0.0.1:29510", "localhost:29510", "[::1]:29510", "[::1]"] {
            assert!(host_ok(Some(h), 29510), "{h}");
        }
        for h in ["evil.example", "evil.example:29510", "127.0.0.1:80", "localhost.evil.example", "192.168.1.5:29510"] {
            assert!(!host_ok(Some(h), 29510), "{h}");
        }
        assert!(!host_ok(None, 29510));
    }
}
