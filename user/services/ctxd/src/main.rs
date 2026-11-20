//! `ctxd`: the context service and its command line (DESIGN §2).
#![forbid(unsafe_code)]

use ctxd::config::{Config, RootCfg};
use ctxd::engine::{Caller, Engine, EngineOpts};
use ctxd::{api, eval, mcp, sock};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const USAGE: &str = "ctxd: context instead of folders (BitsOS CX-1 prototype, CX-5 service)

usage: ctxd [--config FILE] [--data DIR] [--root PATH[:private]]... [--embedder llama|fake|none] COMMAND

commands:
  serve [--port N] [--no-mcp]   run the service: socket, MCP on 127.0.0.1 (default 29510), indexing
  serve --system                BitsOS V2's service: /data/appdata/ctxd, settings context.*, no TCP
  --system COMMAND              a command against that running service (only through its socket)
  model-install FILE            check the e5-small model file and copy it into the data folder
  index                         one indexing run, then exit
  search QUERY [--limit N] [--mcp] [--json]
  status | ws-list | ws-open NAME | explain DOC QUERY | forget SCOPE [ID]
  call OP [JSON-ARGS] [--mcp]   any operation (context_search, workspace_open, ...)
  eval [--topics FILE] [--json] [--fusion linear|rrf]   recall@10 etc. on host/cx-eval
  replay FILE                   recorded sessions against the index, ranks before and after (CX-3)
  extract FILE                  what the extractor sees
  verify                        check the data folder (CURRENT points to a complete generation)

Without a configured root nothing is indexed (eval names host/cx-eval/docs itself). The config lives in
$XDG_CONFIG_HOME/ctxd/config.toml (keys under [context], see DESIGN.md).";

struct Args {
    /// `--system` before the command: the V2 service's folder and socket (`ctxd --system status`).
    system: bool,
    config: Option<PathBuf>,
    data: Option<PathBuf>,
    roots: Vec<RootCfg>,
    embedder: Option<String>,
    rest: Vec<String>,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args { system: false, config: None, data: None, roots: vec![], embedder: None, rest: vec![] };
    let mut it = std::env::args().skip(1);
    while let Some(x) = it.next() {
        let mut val = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        match x.as_str() {
            "--config" => a.config = Some(val("--config")?.into()),
            "--data" => a.data = Some(val("--data")?.into()),
            "--system" => a.system = true,
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

/// CLI against the V2 service: only through its socket (an engine here would index beside it).
static SYSTEM: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Through the socket of a running `ctxd serve`, else with an engine in this process.
fn op_call(cfg: &Config, data: &Path, op: &str, args: Value, caller: Caller) -> Result<Value, String> {
    let sp = cfg.socket_path(data);
    if SYSTEM.load(std::sync::atomic::Ordering::Relaxed) {
        let gone = |e: String| format!("Der Kontextdienst antwortet nicht ({}): {e}", sp.display());
        if caller == Caller::Mcp {
            // What an AI would get: the MCP door through the socket (op mcp).
            let rpc = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": op, "arguments": args}});
            let v = sock::request(&sp, "mcp", rpc, Duration::from_secs(120)).map_err(gone)?;
            let r = &v["result"]["result"];
            return if r["isError"] == json!(false) { Ok(r["structuredContent"].clone()) } else { Err(r.to_string()) };
        }
        let v = sock::request(&sp, op, args, Duration::from_secs(120)).map_err(gone)?;
        return if v["ok"] == json!(true) { Ok(v["result"].clone()) } else { Err(v["error"].to_string()) };
    }
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

/// The socket, MCP on 127.0.0.1 when `mcp` names a port, the indexer and the file events; never returns
/// unless binding fails. `watch_settings`: the V2 service's settings look (system.rs) with what runs now.
fn serve(cfg: &Config, data: &Path, mcp: Option<u16>, watch_settings: Option<(ctxd::config::Context, PathBuf)>) -> Result<i32, String> {
    let sp = cfg.socket_path(data);
    let listener = sock::bind(&sp)?;
    let mcp_l = match mcp {
        Some(port) => Some((mcp::bind(port)?, port)),
        None => None,
    };
    let e = Arc::new(open_engine(cfg, data)?);
    eprintln!("ctxd: data {}  socket {}", data.display(), sp.display());
    if let Some(err) = &e.embedder_error {
        eprintln!("ctxd: embedder unavailable ({err}); keyword search only");
    }
    if let Some((l, port)) = mcp_l {
        eprintln!("ctxd: MCP on http://127.0.0.1:{port}/mcp");
        let e2 = Arc::clone(&e);
        std::thread::Builder::new().name("ctxd-mcp-accept".into()).spawn(move || mcp::serve(e2, l)).map_err(|x| x.to_string())?;
    }
    let e3 = Arc::clone(&e);
    std::thread::Builder::new().name("ctxd-indexer".into()).spawn(move || e3.indexer_loop()).map_err(|x| x.to_string())?;
    if cfg.context.index.watch {
        let (roots, _) = ctxd::roots::resolve(&cfg.context.roots, &cfg.context.forbidden_roots);
        let _ = ctxd::watch::start(Arc::clone(&e), roots, data.to_path_buf());
    }
    if let Some((base, sock)) = watch_settings {
        // DB4: the memory's switches follow settingsd without a restart.
        let (e4, s4) = (Arc::clone(&e), sock.clone());
        std::thread::Builder::new().name("ctxd-habits".into()).spawn(move || ctxd::habits_settings::watch(e4, s4)).map_err(|x| x.to_string())?;
        let running = ctxd::system::fingerprint(&cfg.context);
        std::thread::Builder::new()
            .name("ctxd-settings".into())
            .spawn(move || ctxd::system::watch(base, sock, running))
            .map_err(|x| x.to_string())?;
    }
    ctxd::event!("info", "service.started", port = mcp.unwrap_or(0), roots = cfg.context.roots.len(), embedder = e.embedder.as_ref().map(|x| x.id().to_string()));
    sock::serve(e, listener);
    Ok(0)
}

fn system_data() -> PathBuf {
    ctxd::knobs::var("CTXD_TEST_DATA").map_or_else(|| PathBuf::from(ctxd::system::DATA), PathBuf::from)
}

/// `ctxd serve --system`: the V2 service (system.rs). The test build may point it at another folder and
/// settings socket (`CTXD_TEST_DATA`, `CTXD_TEST_SETTINGS_SOCKET`); the shipped one only at the system's.
fn serve_system() -> Result<i32, String> {
    ctxd::system::refuse_root()?;
    let data = system_data();
    let sock = ctxd::knobs::var("CTXD_TEST_SETTINGS_SOCKET").map_or_else(|| PathBuf::from(ctxd::settings::SOCKET), PathBuf::from);
    ctxd::system::wait_for_data(&data);
    ctxd::util::private_dir(&data).map_err(|e| format!("{}: {e}", data.display()))?;
    ctxd::event::open_sink(&data);
    ctxd::event::CONSOLE.store(true, std::sync::atomic::Ordering::Relaxed);
    let base = ctxd::system::base(&data);
    let (ctx, why) = ctxd::system::configured(&base, &sock);
    if let Some(w) = why {
        ctxd::event!("warn", "settings.unavailable", reason = w);
    }
    if !ctx.embedder.model.as_deref().is_some_and(Path::is_file) && ctx.embedder.kind == "llama" {
        ctxd::event!("warn", "embedder.model_missing", path = ctx.embedder.model.as_ref().map(|p| p.display().to_string()), fallback = "keyword_only");
    }
    let cfg = Config { context: ctx };
    serve(&cfg, &data, None, Some((base, sock)))
}

/// What `--system` may do: talk to the running service through its socket (`serve` is the service itself).
const SOCKET_COMMANDS: [&str; 8] = ["serve", "status", "search", "ws-list", "ws-open", "explain", "forget", "call"];

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
    let mut data = cfg.data_dir(a.data.as_deref());
    if a.system {
        // Root's files in uid 1000's 0700 folder would lock the service out of its own index; and a command
        // that opens the index itself would write beside the running service (eval: the test set into it).
        ctxd::system::refuse_root()?;
        if !SOCKET_COMMANDS.contains(&cmd.as_str()) {
            return Err(format!(
                "„{cmd}“ geht nicht mit --system: den laufenden Dienst erreichen nur {}.",
                SOCKET_COMMANDS.join(", ")
            ));
        }
        data = system_data();
        cfg.context = ctxd::system::base(&data);
        SYSTEM.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    let pos = positional(rest);
    match cmd.as_str() {
        "serve" if flag(rest, "--system") => serve_system(),
        "serve" => {
            let port = opt(rest, "--port").and_then(|p| p.parse().ok()).unwrap_or(cfg.context.mcp.port);
            let mcp = (cfg.context.mcp.enabled && !flag(rest, "--no-mcp")).then_some(port);
            serve(&cfg, &data, mcp, None)
        }
        "model-install" => {
            let f = pos.first().ok_or("model-install FILE")?;
            let d = if a.data.is_some() && !a.system { data.clone() } else { system_data() };
            if d == system_data() {
                ctxd::system::refuse_root()?;
            }
            println!("{}", ctxd::system::install_model(Path::new(f), &d)?);
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
            if cfg.context.roots.is_empty() {
                cfg.context.roots = vec![RootCfg { path: ctxd::config::eval_docs_dir(), private: false }];
            }
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
        "replay" => {
            let f = pos.first().ok_or("replay FILE")?;
            let e = open_engine(&cfg, &data)?;
            e.index_now()?;
            print(&ctxd::replay::run(&e, Path::new(f))?);
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
