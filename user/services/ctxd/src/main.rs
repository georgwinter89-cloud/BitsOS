//! `ctxd`: the context service and its command line (DESIGN §2).
#![forbid(unsafe_code)]

use ctxd::config::{Config, RootCfg};
use ctxd::engine::{Caller, Engine, EngineOpts};
use ctxd::{api, eval, mcp, sock};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const USAGE: &str = "ctxd: context instead of folders (BitsOS CX-1 prototype)

usage: ctxd [--config FILE] [--data DIR] [--root PATH[:private]]... [--embedder llama|fake|none] COMMAND

commands:
  serve [--port N] [--no-mcp]   run the service: socket, MCP on 127.0.0.1 (default 29510), indexing
  index                         one indexing run, then exit
  search QUERY [--limit N] [--mcp] [--json]
  status | ws-list | ws-open NAME | explain DOC QUERY | forget SCOPE [ID]
  call OP [JSON-ARGS] [--mcp]   any operation (context_search, workspace_open, ...)
  eval [--topics FILE] [--json] [--fusion linear|rrf]   recall@10 etc. on host/cx-eval
  extract FILE                  what the extractor sees
  verify                        check the data folder (CURRENT points to a complete generation)

Without a config file the only root is host/cx-eval/docs. The config lives in
$XDG_CONFIG_HOME/ctxd/config.toml (keys under [context], see DESIGN.md).";

struct Args {
    config: Option<PathBuf>,
    data: Option<PathBuf>,
    roots: Vec<RootCfg>,
    embedder: Option<String>,
    rest: Vec<String>,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args { config: None, data: None, roots: vec![], embedder: None, rest: vec![] };
    let mut it = std::env::args().skip(1);
    while let Some(x) = it.next() {
        let mut val = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        match x.as_str() {
            "--config" => a.config = Some(val("--config")?.into()),
            "--data" => a.data = Some(val("--data")?.into()),
            "--embedder" => a.embedder = Some(val("--embedder")?),
            "--root" => {
                let v = val("--root")?;
                let (p, private) = match v.strip_suffix(":private") {
                    Some(p) => (p.to_string(), true),
                    None => (v, false),
                };
                a.roots.push(RootCfg { path: p.into(), private });
            }
            "-h" | "--help" => return Err(String::new()),
            _ => {
                a.rest.push(x);
                a.rest.extend(it.by_ref());
            }
        }
    }
    Ok(a)
}

fn flag(rest: &[String], name: &str) -> bool {
    rest.iter().any(|r| r == name)
}

fn opt(rest: &[String], name: &str) -> Option<String> {
    rest.iter().position(|r| r == name).and_then(|i| rest.get(i + 1)).cloned()
}

fn positional(rest: &[String]) -> Vec<String> {
    let with_value = ["--limit", "--port", "--topics", "--fusion"];
    let mut out = Vec::new();
    let mut skip = false;
    for r in rest {
        if skip {
            skip = false;
        } else if with_value.contains(&r.as_str()) {
            skip = true;
        } else if !r.starts_with("--") {
            out.push(r.clone());
        }
    }
    out
}

fn print(v: &Value) {
    println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
}

fn open_engine(cfg: &Config, data: &Path) -> Result<Engine, String> {
    let exe = std::env::current_exe().ok();
    Engine::open(cfg.context.clone(), data, EngineOpts { worker_exe: exe })
}

/// Through the socket of a running `ctxd serve`, else with an engine in this process.
fn op_call(cfg: &Config, data: &Path, op: &str, args: Value, caller: Caller) -> Result<Value, String> {
    let sp = cfg.socket_path(data);
    if caller == Caller::Desktop
        && sp.exists()
        && let Ok(v) = sock::request(&sp, op, args.clone(), Duration::from_secs(120))
    {
        return if v["ok"] == json!(true) { Ok(v["result"].clone()) } else { Err(v["error"].to_string()) };
    }
    let e = open_engine(cfg, data)?;
    if e.generation().is_none() {
        e.index_now()?;
    }
    api::call(&e, op, &args, caller).map_err(|x| format!("{}: {}", x.code, x.message))
}

fn run() -> Result<i32, String> {
    let a = match parse_args() {
        Ok(a) => a,
        Err(e) if e.is_empty() => {
            println!("{USAGE}");
            return Ok(0);
        }
        Err(e) => return Err(e),
    };
    let Some(cmd) = a.rest.first().cloned() else {
        println!("{USAGE}");
        return Ok(2);
    };
    let rest = &a.rest[1..];
    if cmd == "extract-worker" {
        let mem = rest.first().and_then(|m| m.parse().ok()).unwrap_or(0);
        let cpu = rest.get(1).and_then(|m| m.parse().ok()).unwrap_or(0);
        return Ok(ctxd::extract::pool::worker_main(mem, cpu));
    }
    let mut cfg = Config::load(a.config.as_deref())?;
    if !a.roots.is_empty() {
        cfg.context.roots = a.roots.clone();
    }
    if let Some(k) = &a.embedder {
        cfg.context.embedder.kind = k.clone();
    }
    if let Some(f) = opt(rest, "--fusion") {
        cfg.context.rank.fusion = f;
    }
    let data = cfg.data_dir(a.data.as_deref());
    let pos = positional(rest);
    match cmd.as_str() {
        "serve" => {
            let port = opt(rest, "--port").and_then(|p| p.parse().ok()).unwrap_or(cfg.context.mcp.port);
            let sp = cfg.socket_path(&data);
            let listener = sock::bind(&sp)?;
            let mcp_l = if cfg.context.mcp.enabled && !flag(rest, "--no-mcp") { Some(mcp::bind(port)?) } else { None };
            let e = Arc::new(open_engine(&cfg, &data)?);
            eprintln!("ctxd: data {}  socket {}", data.display(), sp.display());
            if let Some(err) = &e.embedder_error {
                eprintln!("ctxd: embedder unavailable ({err}); keyword search only");
            }
            if let Some(l) = mcp_l {
                eprintln!("ctxd: MCP on http://127.0.0.1:{port}/mcp");
                let e2 = Arc::clone(&e);
                std::thread::Builder::new()
                    .name("ctxd-mcp-accept".into())
                    .spawn(move || mcp::serve(e2, l))
                    .map_err(|x| x.to_string())?;
            }
            let e3 = Arc::clone(&e);
            std::thread::Builder::new()
                .name("ctxd-indexer".into())
                .spawn(move || e3.indexer_loop())
                .map_err(|x| x.to_string())?;
            if cfg.context.index.watch {
                let (roots, _) = ctxd::roots::resolve(&cfg.context.roots, &cfg.context.forbidden_roots);
                let _ = ctxd::watch::start(Arc::clone(&e), roots, data.clone());
            }
            ctxd::event!("info", "service.started", port = port);
            sock::serve(e, listener);
            Ok(0)
        }
        "index" => {
            let e = open_engine(&cfg, &data)?;
            let o = e.index_now()?;
            print(&serde_json::to_value(&o).unwrap_or_default());
            Ok(0)
        }
        "search" => {
            let q = pos.first().ok_or("search needs a query")?;
            let limit: usize = opt(rest, "--limit").and_then(|l| l.parse().ok()).unwrap_or(10);
            let caller = if flag(rest, "--mcp") { Caller::Mcp } else { Caller::Desktop };
            let v = op_call(&cfg, &data, "context_search", json!({"query": q, "limit": limit}), caller)?;
            if flag(rest, "--json") {
                print(&v);
            } else {
                if v["partial"] == json!(true) {
                    println!("(nur Stichwortsuche: {})", v["partial_reason"].as_str().unwrap_or(""));
                }
                for (i, it) in v["items"].as_array().into_iter().flatten().enumerate() {
                    println!(
                        "{:2}. {:.3}  {}  [{}]  {}",
                        i + 1,
                        it["score"].as_f64().unwrap_or(0.0),
                        it["title"].as_str().unwrap_or(""),
                        it["kind"].as_str().unwrap_or(""),
                        it["path"].as_str().unwrap_or("")
                    );
                    if let Some(s) = it["snippet"].as_str() {
                        println!("      {s}");
                    }
                }
                println!("({} ms, {})", v["took_ms"], v["embedder"].as_str().unwrap_or("keyword only"));
            }
            Ok(0)
        }
        "status" => {
            print(&op_call(&cfg, &data, "context_status", json!({}), Caller::Desktop)?);
            Ok(0)
        }
        "ws-list" => {
            print(&op_call(&cfg, &data, "workspace_list", json!({}), Caller::Desktop)?);
            Ok(0)
        }
        "ws-open" => {
            let n = pos.first().ok_or("ws-open needs a name")?;
            print(&op_call(&cfg, &data, "workspace_open", json!({"name": n}), Caller::Desktop)?);
            Ok(0)
        }
        "explain" => {
            let (d, q) = (pos.first().ok_or("explain DOC QUERY")?, pos.get(1).ok_or("explain DOC QUERY")?);
            print(&op_call(&cfg, &data, "context_explain", json!({"doc": d, "query": q}), Caller::Desktop)?);
            Ok(0)
        }
        "forget" => {
            let scope = pos.first().ok_or("forget SCOPE [ID]")?;
            let mut args = json!({"scope": scope});
            if let Some(id) = pos.get(1) {
                args[if scope == "workspace" { "workspace" } else { "doc" }] = json!(id);
            }
            print(&op_call(&cfg, &data, "context_forget", args, Caller::Desktop)?);
            Ok(0)
        }
        "call" => {
            let op = pos.first().ok_or("call OP [JSON]")?;
            let args: Value = match pos.get(1) {
                Some(j) => serde_json::from_str(j).map_err(|e| format!("args: {e}"))?,
                None => json!({}),
            };
            let caller = if flag(rest, "--mcp") { Caller::Mcp } else { Caller::Desktop };
            print(&op_call(&cfg, &data, op, args, caller)?);
            Ok(0)
        }
        "eval" => {
            let topics = opt(rest, "--topics").map(PathBuf::from).unwrap_or_else(ctxd::config::eval_topics_file);
            let topics = eval::load_topics(&topics)?;
            let e = open_engine(&cfg, &data)?;
            let o = e.index_now()?;
            let mut rep = eval::evaluate(&e, &topics);
            rep["index"] = serde_json::to_value(&o).unwrap_or_default();
            if flag(rest, "--json") {
                print(&rep);
            } else {
                print!("{}", eval::table(&rep));
                println!(
                    "index: {} docs, {} chunks, {:.1} s; search p50 {} ms; partial {}",
                    rep["docs"], rep["chunks"], o.seconds, rep["search_ms"]["p50"], rep["partial_queries"]
                );
            }
            Ok(0)
        }
        "extract" => {
            let f = pos.first().ok_or("extract FILE")?;
            let ex = ctxd::extract::pool::extract_path(std::path::Path::new(f), ctxd::extract::Limits::default())?;
            print(&serde_json::to_value(&ex).unwrap_or_default());
            Ok(0)
        }
        "verify" => {
            let st = ctxd::store::Store::open(&data, false)?;
            match ctxd::engine::Generation::load(&st, usize::MAX) {
                Ok(Some(g)) => {
                    let mut docs: Vec<(String, String, u32)> =
                        g.cat.docs.iter().map(|d| (d.paths[0].path.clone(), d.hash.clone(), d.chunk_count)).collect();
                    docs.sort();
                    let content =
                        ctxd::store::Store::bytes_sum(serde_json::to_string(&docs).unwrap_or_default().as_bytes());
                    print(&json!({"ok": true, "generation": g.n, "docs": g.cat.docs.len(), "chunks": g.cat.n_chunks,
                        "content_sum": content, "chunks_sum": g.cat.chunks_sum, "vectors_sum": g.cat.vectors_sum,
                        "keyword_docs": g.kidx.num_docs()}));
                    Ok(0)
                }
                Ok(None) => {
                    print(&json!({"ok": true, "generation": null}));
                    Ok(0)
                }
                Err(e) => {
                    print(&json!({"ok": false, "error": e}));
                    Ok(1)
                }
            }
        }
        other => Err(format!("unknown command {other:?} (ctxd --help)")),
    }
}

fn main() {
    match run() {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("ctxd: {e}");
            std::process::exit(1);
        }
    }
}
